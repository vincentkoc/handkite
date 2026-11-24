//! On-host Vision recognition. Pixels and recognised text never leave this process.
use super::{AdapterError, targets_pixels::Pixels};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Text {
    pub text: String,
    /// Pixel x/y/width/height, with top-left origin.
    pub rect: [f64; 4],
}

#[derive(Default)]
pub(super) struct Cache {
    identity: String,
    dimensions: [u32; 2],
    image_digest: Option<[u8; 32]>,
    rows: Vec<Text>,
    last_failure: Option<Failure>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Failure {
    Busy,
    TimedOut,
    Failed,
}

fn intersects(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

impl Cache {
    pub(super) fn recognize(
        &mut self,
        identity: &str,
        image: &Pixels,
    ) -> Result<Vec<Text>, AdapterError> {
        self.recognize_with(identity, image, recognize)
    }

    fn recognize_with(
        &mut self,
        identity: &str,
        image: &Pixels,
        recognize: impl FnOnce(&Pixels, &[[f64; 4]]) -> Result<Vec<Text>, Failure>,
    ) -> Result<Vec<Text>, AdapterError> {
        self.last_failure = None;
        let mut failure = None;
        let result = self.update(identity, image, |image, regions| {
            recognize(image, regions).map_err(|reason| {
                failure = Some(reason);
                AdapterError
            })
        });
        self.last_failure = failure;
        result
    }

    pub(super) fn last_failure(&self) -> Option<Failure> {
        self.last_failure
    }

    pub(super) fn begin_attempt(&mut self) {
        self.last_failure = None;
    }

    fn update(
        &mut self,
        identity: &str,
        image: &Pixels,
        recognize: impl FnOnce(&Pixels, &[[f64; 4]]) -> Result<Vec<Text>, AdapterError>,
    ) -> Result<Vec<Text>, AdapterError> {
        let digest = ring::digest::digest(&ring::digest::SHA256, &image.rgba)
            .as_ref()
            .try_into()
            .expect("SHA256");
        if self.identity == identity
            && self.dimensions == [image.width, image.height]
            && self.image_digest == Some(digest)
        {
            return Ok(self.rows.clone());
        }
        // Any changed pixel rechecks the complete frame. Partial crops can lose
        // edge text or leave a previously missed row absent from later results.
        let mut rows = recognize(
            image,
            &[[0.0, 0.0, image.width as f64, image.height as f64]],
        )?;
        normalize_and_validate(&mut rows, image.width, image.height)?;
        self.identity = identity.into();
        self.dimensions = [image.width, image.height];
        self.image_digest = Some(digest);
        self.rows = rows.clone();
        Ok(rows)
    }
}

fn normalize_and_validate(rows: &mut [Text], width: u32, height: u32) -> Result<(), AdapterError> {
    const EDGE_EPSILON: f64 = 0.001;
    if rows.len() > 2048 {
        return Err(AdapterError);
    }
    for row in rows {
        let [x, y, w, h] = row.rect;
        if row.text.is_empty()
            || row.text.len() > 16_384
            || row.text.chars().any(char::is_control)
            || !row.rect.iter().all(|n| n.is_finite())
            || w <= 0.0
            || h <= 0.0
            || x < -EDGE_EPSILON
            || y < -EDGE_EPSILON
            || x + w > width as f64 + EDGE_EPSILON
            || y + h > height as f64 + EDGE_EPSILON
        {
            return Err(AdapterError);
        }
        // Vision's normalized boxes can drift just outside the raster after
        // conversion to pixels. Clip only those edges, preserving interior boxes.
        for (axis, limit) in [width, height].into_iter().enumerate() {
            let start = row.rect[axis];
            let end = start + row.rect[axis + 2];
            if start < 0.0 || end > limit as f64 {
                row.rect[axis] = start.max(0.0);
                row.rect[axis + 2] = end.min(limit as f64) - row.rect[axis];
            }
        }
        if row.rect[2] <= 0.0 || row.rect[3] <= 0.0 {
            return Err(AdapterError);
        }
    }
    Ok(())
}

/// Deliberately bypasses the image cache for input admission and effect proof.
pub(super) fn fresh(image: &Pixels) -> Result<Vec<Text>, AdapterError> {
    recognize(
        image,
        &[[0.0, 0.0, image.width as f64, image.height as f64]],
    )
    .map_err(|_| AdapterError)
}

pub(super) fn same_text(rows: &[Text], expected: &Text) -> bool {
    matching_text(rows, expected).is_some()
}

pub(super) fn matching_text<'a>(rows: &'a [Text], expected: &Text) -> Option<&'a Text> {
    let mut nearby = rows
        .iter()
        .filter(|row| row.text == expected.text && intersects(row.rect, expected.rect));
    let matched = nearby.next()?;
    (nearby.next().is_none()
        && matched
            .rect
            .iter()
            .zip(expected.rect)
            .all(|(a, b)| (a - b).abs() <= 3.0))
    .then_some(matched)
}

pub(super) fn moved_text(before: &[Text], after: &[Text], expected: &Text) -> bool {
    if after
        .iter()
        .any(|row| row.text.contains(&expected.text) && intersects(row.rect, expected.rect))
    {
        return false;
    }
    let new_matches: Vec<_> = after
        .iter()
        .filter(|row| {
            row.text.contains(&expected.text)
                && !before.iter().any(|old| {
                    old.text == row.text
                        && old
                            .rect
                            .iter()
                            .zip(row.rect)
                            .all(|(a, b)| (a - b).abs() <= 3.0)
                })
        })
        .collect();
    new_matches.len() == 1
}

pub(super) fn safe_context(rows: &[Text]) -> bool {
    const BLOCKERS: &[&str] = &[
        "enter passcode",
        "enter password",
        "verification code",
        "face id",
        "touch id",
        "请输入密码",
        "输入锁屏密码",
        "验证码",
    ];
    !rows.iter().any(|row| {
        let text = row.text.to_lowercase();
        BLOCKERS.iter().any(|marker| text.contains(marker))
    })
}

#[cfg(target_os = "macos")]
fn recognize(image: &Pixels, regions: &[[f64; 4]]) -> Result<Vec<Text>, Failure> {
    unsafe extern "C" {
        fn handkite_recognize_rgba(
            bytes: *const u8,
            width: usize,
            height: usize,
            rects: *const f64,
            count: usize,
            status: *mut libc::c_int,
        ) -> *mut libc::c_char;
    }
    let _span = crate::timing::Span::new("ocr_vision");
    // The synchronous native call borrows both bounded buffers and transfers
    // one malloc-owned, NUL-terminated JSON result back to this scope.
    let mut status = 0;
    let raw = unsafe {
        handkite_recognize_rgba(
            image.rgba.as_ptr(),
            image.width as usize,
            image.height as usize,
            regions.as_ptr().cast(),
            regions.len(),
            &mut status,
        )
    };
    let result = unsafe { consume_result(raw, status) };
    if let Err(reason) = result {
        crate::timing::outcome(
            match reason {
                Failure::Busy => "ocr_busy",
                Failure::TimedOut => "ocr_timed_out",
                Failure::Failed => "ocr_failed",
            },
            false,
        );
    }
    let mut rows = result?;
    normalize_and_validate(&mut rows, image.width, image.height).map_err(|_| Failure::Failed)?;
    Ok(rows)
}

#[cfg(not(target_os = "macos"))]
fn recognize(_: &Pixels, _: &[[f64; 4]]) -> Result<Vec<Text>, Failure> {
    Err(Failure::Failed)
}

#[cfg(target_os = "macos")]
unsafe fn consume_result(
    raw: *mut libc::c_char,
    status: libc::c_int,
) -> Result<Vec<Text>, Failure> {
    // Consume the allocation on every status; failed or late work is never parsed.
    let result = match status {
        1 if !raw.is_null() => {
            serde_json::from_slice(unsafe { std::ffi::CStr::from_ptr(raw).to_bytes() })
                .map_err(|_| Failure::Failed)
        }
        2 => Err(Failure::Busy),
        3 => Err(Failure::TimedOut),
        _ => Err(Failure::Failed),
    };
    unsafe { libc::free(raw.cast()) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_failure_never_updates_cached_rows_and_cannot_leak_into_next_attempt() {
        let mut cache = Cache::default();
        let before = image();
        cache
            .recognize_with("same", &before, |_, _| Ok(vec![]))
            .unwrap();
        let digest = cache.image_digest;
        let mut after = image();
        after.rgba[0] = 0;
        assert!(
            cache
                .recognize_with("same", &after, |_, _| Err(Failure::TimedOut))
                .is_err()
        );
        assert_eq!(cache.last_failure(), Some(Failure::TimedOut));
        assert_eq!(cache.image_digest, digest);
        cache.begin_attempt();
        assert_eq!(cache.last_failure(), None);
        assert!(
            cache
                .recognize_with("same", &before, |_, _| panic!(
                    "exact frame should reuse cache"
                ))
                .is_ok()
        );
        assert_eq!(cache.last_failure(), None);
        assert!(
            cache
                .recognize_with("same", &after, |_, _| Err(Failure::Busy))
                .is_err()
        );
        assert_eq!(cache.last_failure(), Some(Failure::Busy));
        assert_eq!(cache.image_digest, digest);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_status_rejects_late_output_and_distinguishes_timeout_from_busy() {
        unsafe fn allocated(bytes: &[u8]) -> *mut libc::c_char {
            let ptr = unsafe { libc::malloc(bytes.len()) }.cast::<libc::c_char>();
            assert!(!ptr.is_null());
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast(), bytes.len()) };
            ptr
        }
        for (status, reason) in [
            (0, Failure::Failed),
            (2, Failure::Busy),
            (3, Failure::TimedOut),
            (99, Failure::Failed),
        ] {
            // Invalid JSON proves failed native output is freed without being parsed.
            assert_eq!(
                unsafe { consume_result(allocated(b"invalid\0"), status) }.unwrap_err(),
                reason
            );
        }
        assert_eq!(
            unsafe { consume_result(std::ptr::null_mut(), 1) }.unwrap_err(),
            Failure::Failed
        );
        assert!(
            unsafe { consume_result(allocated(b"[]\0"), 1) }
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            unsafe { consume_result(allocated(b"invalid\0"), 1) }.unwrap_err(),
            Failure::Failed
        );
    }
    fn image() -> Pixels {
        Pixels {
            width: 768,
            height: 768,
            rgba: vec![255; 768 * 768 * 4],
        }
    }
    #[test]
    fn exact_image_reuse_and_any_change_rechecks_the_full_frame() {
        let mut cache = Cache::default();
        let mut pixels = image();
        cache
            .update("session/app", &pixels, |_, r| {
                assert_eq!(r, &[[0., 0., 768., 768.]]);
                Ok(vec![Text {
                    text: "District".into(),
                    rect: [500., 500., 100., 20.],
                }])
            })
            .unwrap();
        assert_eq!(
            cache
                .update("session/app", &pixels, |_, _| panic!("unchanged OCR rerun"))
                .unwrap()
                .len(),
            1
        );
        pixels.rgba[0] = 0;
        let result = cache
            .update("session/app", &pixels, |_, r| {
                assert_eq!(r, &[[0., 0., 768., 768.]]);
                Ok(vec![])
            })
            .unwrap();
        assert!(result.is_empty());
        cache
            .update("new/app", &pixels, |_, r| {
                assert_eq!(r, &[[0., 0., 768., 768.]]);
                Ok(vec![])
            })
            .unwrap();
    }
    #[test]
    fn full_frame_updates_recover_missed_rows_and_do_not_clip_old_crop_edges() {
        let mut cache = Cache::default();
        let mut pixels = image();
        let boundary = Text {
            text: "Boundary".into(),
            rect: [380., 300., 80., 20.],
        };
        cache
            .update("session/app", &pixels, |_, _| Ok(vec![boundary.clone()]))
            .unwrap();
        pixels.rgba[0] = 0;
        let rows = cache
            .update("session/app", &pixels, |_, regions| {
                // The old dirty-tile crop ended at x=384 and excluded the distant row.
                assert_eq!(regions, &[[0., 0., 768., 768.]]);
                Ok(vec![
                    boundary.clone(),
                    Text {
                        text: "Previously missed".into(),
                        rect: [600., 600., 100., 20.],
                    },
                ])
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].rect, boundary.rect);
        assert_eq!(rows[1].text, "Previously missed");
        let reused = cache
            .update("session/app", &pixels, |_, _| panic!("identical frame"))
            .unwrap();
        assert_eq!(reused.len(), 2);
        pixels.width = 384;
        pixels.height = 1536;
        assert!(
            cache
                .update("session/app", &pixels, |_, regions| {
                    assert_eq!(regions, &[[0., 0., 384., 1536.]]);
                    Ok(vec![])
                })
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn invalid_results_and_authentication_prompts_refuse() {
        assert!(
            normalize_and_validate(
                &mut [Text {
                    text: "row".into(),
                    rect: [f64::NAN, 0., 10., 10.]
                }],
                100,
                100
            )
            .is_err()
        );
        assert!(!safe_context(&[Text {
            text: "Enter Passcode".into(),
            rect: [0., 0., 10., 10.]
        }]));
        assert!(safe_context(&[Text {
            text: "南山区".into(),
            rect: [0., 0., 10., 10.]
        }]));
    }
    #[test]
    fn numerical_edge_drift_is_clipped_without_changing_interior_boxes() {
        let interior = [0.1, 0.2, 10.3, 10.4];
        let mut rows: Vec<_> = [
            [-0.000_000_052_611_213, 20., 10., 10.],
            [20., -0.001, 10., 10.],
            [90., 20., 10.001, 10.],
            [20., 90., 10., 10.001],
            interior,
        ]
        .into_iter()
        .map(|rect| Text {
            text: "row".into(),
            rect,
        })
        .collect();
        normalize_and_validate(&mut rows, 100, 100).unwrap();
        assert_eq!(rows[0].rect, [0., 20., 9.999_999_947_388_786, 10.]);
        assert_eq!(rows[1].rect, [20., 0., 10., 9.999]);
        assert_eq!(rows[2].rect, [90., 20., 10., 10.]);
        assert_eq!(rows[3].rect, [20., 90., 10., 10.]);
        assert_eq!(rows[4].rect, interior);
        let normalized: Vec<_> = rows.iter().map(|row| row.rect).collect();
        normalize_and_validate(&mut rows, 100, 100).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.rect).collect::<Vec<_>>(),
            normalized
        );
    }

    #[test]
    fn edge_normalization_refuses_real_overflow_nonfinite_and_empty_boxes() {
        for rect in [
            [-0.001_001, 20., 10., 10.],
            [20., -0.001_001, 10., 10.],
            [90., 20., 10.001_001, 10.],
            [20., 90., 10., 10.001_001],
            [f64::NAN, 20., 10., 10.],
            [20., f64::INFINITY, 10., 10.],
            [20., 20., f64::INFINITY, 10.],
            [20., 20., 10., f64::NEG_INFINITY],
            [20., 20., 0., 10.],
            [20., 20., 10., 0.],
            [20., 20., -0.000_1, 10.],
            [-0.001, 20., 0.001, 10.],
            [100., 20., 0.001, 10.],
            [20., -0.001, 10., 0.001],
            [20., 100., 10., 0.001],
        ] {
            assert!(
                normalize_and_validate(
                    &mut [Text {
                        text: "row".into(),
                        rect,
                    }],
                    100,
                    100,
                )
                .is_err(),
                "{rect:?}"
            );
        }
    }

    #[test]
    fn cache_keeps_normalized_edges_and_does_not_admit_failed_updates() {
        let mut cache = Cache::default();
        let mut pixels = image();
        let result = cache
            .update("session/app", &pixels, |_, _| {
                Ok(vec![Text {
                    text: "row".into(),
                    rect: [-0.000_1, 20., 10., 10.],
                }])
            })
            .unwrap();
        assert_eq!(result[0].rect, [0., 20., 9.9999, 10.]);
        let reused = cache
            .update("session/app", &pixels, |_, _| panic!("unchanged OCR rerun"))
            .unwrap();
        assert_eq!(reused[0].rect, result[0].rect);
        pixels.rgba[0] = 0;
        assert!(
            cache
                .update("session/app", &pixels, |_, _| {
                    Ok(vec![Text {
                        text: "row".into(),
                        rect: [-0.002, 20., 10., 10.],
                    }])
                })
                .is_err()
        );
        assert_eq!(cache.rows[0].rect, result[0].rect);
        let recovered = cache
            .update("session/app", &pixels, |_, _| Ok(vec![]))
            .unwrap();
        assert!(recovered.is_empty());
    }
    #[test]
    fn no_match_and_ambiguous_text_are_not_positive_effects() {
        let expected = Text {
            text: "District".into(),
            rect: [20., 20., 80., 20.],
        };
        assert!(!same_text(&[], &expected));
        assert!(!same_text(&[expected.clone(), expected.clone()], &expected));
        assert!(!moved_text(&[expected.clone()], &[], &expected));
        let selected = Text {
            text: "City District".into(),
            rect: [20., 200., 100., 20.],
        };
        assert!(moved_text(
            &[expected.clone()],
            &[selected.clone()],
            &expected
        ));
        assert!(!moved_text(
            &[expected.clone()],
            &[selected.clone(), selected],
            &expected
        ));
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn native_vision_accepts_a_blank_bounded_raster() {
        assert!(fresh(&image()).unwrap().is_empty());
    }
}
