use super::response;
use crate::wire::WireObservation;
use handkite_core::protocol::{CaptureMode, DeviceKey, DeviceState, Revision};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum TreeFormat {
    #[default]
    Full,
    Compact,
    Elements,
    Outline,
    Omit,
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ImageMode {
    #[default]
    Always,
    Omit,
    Changed,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(try_from = "UncheckedOptions")]
pub(super) struct Options {
    pub capture: CaptureMode,
    pub targets: bool,
    pub tree_format: TreeFormat,
    pub image: ImageMode,
    pub since_revision: Option<Revision>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedOptions {
    #[serde(default)]
    pub capture: CaptureMode,
    #[serde(default)]
    pub targets: bool,
    #[serde(default, deserialize_with = "present_tree_format")]
    pub tree_format: Option<TreeFormat>,
    #[serde(default, deserialize_with = "present_image")]
    pub image: Option<ImageMode>,
    #[serde(default, deserialize_with = "present_revision")]
    pub since_revision: Option<Revision>,
}

impl TryFrom<UncheckedOptions> for Options {
    type Error = &'static str;

    fn try_from(value: UncheckedOptions) -> Result<Self, Self::Error> {
        let tree_format = value.tree_format.unwrap_or(if value.targets {
            TreeFormat::Elements
        } else {
            TreeFormat::Full
        });
        let image = value.image.unwrap_or(if value.targets {
            ImageMode::Omit
        } else {
            ImageMode::Always
        });
        if value.targets && value.capture == CaptureMode::ImageOnly {
            return Err("targets require paired capture");
        }
        if value.capture == CaptureMode::ImageOnly && tree_format != TreeFormat::Omit {
            return Err("image-only capture requires tree_format=omit");
        }
        if tree_format == TreeFormat::Omit
            && (image != ImageMode::Always || value.since_revision.is_some())
        {
            return Err("tree omission requires an image without a delta baseline");
        }
        Ok(Self {
            capture: value.capture,
            targets: value.targets,
            tree_format,
            image,
            since_revision: value.since_revision,
        })
    }
}

fn present_tree_format<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<TreeFormat>, D::Error> {
    TreeFormat::deserialize(deserializer).map(Some)
}
fn present_image<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<ImageMode>, D::Error> {
    ImageMode::deserialize(deserializer).map(Some)
}

fn present_revision<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Revision>, D::Error> {
    Revision::deserialize(deserializer).map(Some)
}

pub(super) fn schema() -> Value {
    json!({"type":"object", "additionalProperties":false, "properties":{
        "targets":{"type":"boolean","description":"Opt in to one-use opaque native element handles. Requires paired capture; eN presentation IDs cannot be used for input."},
        "capture":{"type":"string","enum":["paired","image_only"],
            "description":"Default paired. image_only skips XML for the returned observation, requires tree_format=omit, and has no state or actionable revision. Actions retain full pre-input admission; Home and Lock retain paired verification."},
        "tree_format":{"type":"string","enum":["full","compact","elements","outline","omit"],
            "description":"elements returns visible nodes with session-local ids and optional added/removed/changed deltas. Response presentation only; internal checks retain the full capture. omit returns an image only and requires image=always without since_revision."},
        "image":{"type":"string","enum":["always","omit","changed"],
            "description":"Include, explicitly omit, or omit an identical acknowledged image. Does not skip internal capture."},
        "since_revision":{"type":"integer","minimum":0,"maximum":u64::MAX,
            "description":"Last response revision actually received in this session. Enables smaller tree deltas; unavailable baselines return a full tree."}
    }})
}

const MAX_BASELINE: usize = 1024 * 1024;
const MAX_DELTA_LINES: usize = 8192;

struct Baseline {
    revision: Revision,
    device: DeviceKey,
    state: DeviceState,
    format: TreeFormat,
    tree: String,
    image_digest: [u8; 32],
    image_known: bool,
}

#[derive(Default)]
pub(super) struct Cache {
    last: Option<Baseline>,
    elements: super::elements::Cache,
}

impl Cache {
    pub(super) fn has_element_baseline(&self, revision: Revision) -> bool {
        self.last
            .as_ref()
            .is_some_and(|b| b.revision == revision && b.format == TreeFormat::Elements)
    }

    pub(super) fn clear(&mut self) {
        self.last = None;
        self.elements.clear();
    }

    /// Element frames from the observation at `revision`, for pacing only.
    pub(super) fn element_rects(&self, revision: Revision) -> Vec<[f64; 4]> {
        self.elements.rects(revision)
    }

    pub(super) fn content(
        &mut self,
        mut observation: WireObservation,
        options: Options,
    ) -> Vec<Value> {
        if options.tree_format == TreeFormat::Omit {
            self.clear();
            let payload = response::project_payload(&mut observation, options.tree_format);
            return response::finish_observation_content(
                payload,
                observation.screenshot_format,
                observation.screenshot_base64,
                true,
            );
        }
        let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
        digest.update(match observation.screenshot_format {
            handkite_core::protocol::ImageFormat::Png => b"png",
            handkite_core::protocol::ImageFormat::Jpeg => b"jpeg",
        });
        digest.update(observation.screenshot_base64.as_bytes());
        let image_digest: [u8; 32] = digest.finish().as_ref().try_into().unwrap();
        if options.tree_format == TreeFormat::Elements {
            let projected = self.elements.project(&observation, options.since_revision);
            let baseline = self.last.as_ref().filter(|last| {
                Some(last.revision) == options.since_revision
                    && last.device == observation.device
                    && last.state == observation.state
                    && last.format == TreeFormat::Elements
            });
            let mut include_image = match options.image {
                ImageMode::Always => true,
                ImageMode::Omit => false,
                ImageMode::Changed => !baseline
                    .is_some_and(|last| last.image_known && last.image_digest == image_digest),
            };
            let mut payload = match projected {
                Ok(payload) => payload,
                Err(reason) => {
                    self.clear();
                    include_image = true;
                    json!({"metadata":response::metadata(&observation), "tree_format":"elements",
                        "elements":[], "actionable":false, "tree_error":reason})
                }
            };
            let usable = payload.get("tree_error").is_none();
            payload["image"] = json!({"included":include_image});
            self.last = usable.then(|| Baseline {
                revision: observation.revision,
                device: observation.device.clone(),
                state: observation.state.clone(),
                format: TreeFormat::Elements,
                tree: String::new(),
                image_digest,
                image_known: include_image || options.image == ImageMode::Changed,
            });
            return response::finish_observation_content(
                payload,
                observation.screenshot_format,
                observation.screenshot_base64,
                include_image,
            );
        }
        let mut payload = response::project_payload(&mut observation, options.tree_format);
        let tree = payload["accessibility_tree"]
            .as_str()
            .expect("projected tree");
        let projected = payload
            .get("tree_format_reason")
            .is_none_or(|reason| reason == "requested");
        let baseline = options.since_revision.and_then(|revision| {
            self.last.as_ref().filter(|last| {
                last.revision == revision
                    && observation.revision.0 > revision.0
                    && last.device == observation.device
                    && last.state == observation.state
                    && last.format == options.tree_format
                    && projected
                    && tree.len() <= MAX_BASELINE
            })
        });
        let include_image = match options.image {
            ImageMode::Always => true,
            ImageMode::Omit => false,
            ImageMode::Changed => {
                !baseline.is_some_and(|last| last.image_known && last.image_digest == image_digest)
            }
        };
        let next = (tree.len() <= MAX_BASELINE && projected).then(|| Baseline {
            revision: observation.revision,
            device: observation.device.clone(),
            state: observation.state.clone(),
            format: options.tree_format,
            tree: tree.into(),
            image_digest,
            image_known: include_image || options.image == ImageMode::Changed,
        });
        if options.since_revision.is_some() {
            let patch = baseline.and_then(|last| line_delta(&last.tree, tree, last.revision));
            if let Some(patch) = patch {
                let mut delta = payload.clone();
                delta.as_object_mut().unwrap().remove("accessibility_tree");
                delta["accessibility_tree_delta"] = patch;
                if encoded_text_len(&delta) < encoded_text_len(&payload) {
                    payload = delta;
                } else {
                    payload["tree_delta_reset"] = json!("not_smaller");
                }
            } else {
                payload["tree_delta_reset"] =
                    json!(if baseline.is_some() || tree.len() > MAX_BASELINE {
                        "limit_exceeded"
                    } else {
                        "baseline_unavailable"
                    });
            }
        }
        if options.image == ImageMode::Changed {
            payload["image"] = if include_image {
                json!({"included":true,"reason":"changed_or_unavailable"})
            } else {
                json!({"included":false,"reason":"unchanged","base_revision":options.since_revision})
            };
        }
        let content = response::finish_observation_content(
            payload,
            observation.screenshot_format,
            observation.screenshot_base64,
            include_image,
        );
        self.last = next;
        content
    }
}

fn encoded_text_len(payload: &Value) -> usize {
    serde_json::to_vec(&json!({"type":"text","text":payload.to_string()}))
        .unwrap()
        .len()
}

fn line_delta(before: &str, after: &str, revision: Revision) -> Option<Value> {
    let before: Vec<_> = before
        .split_inclusive('\n')
        .take(MAX_DELTA_LINES + 1)
        .collect();
    let after: Vec<_> = after
        .split_inclusive('\n')
        .take(MAX_DELTA_LINES + 1)
        .collect();
    if before.len() > MAX_DELTA_LINES || after.len() > MAX_DELTA_LINES {
        return None;
    }
    let prefix = before
        .iter()
        .zip(&after)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = before[prefix..]
        .iter()
        .rev()
        .zip(after[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    Some(json!({"base_revision":revision,"start_line":prefix,
        "delete_lines":before.len() - prefix - suffix,
        "insert":after[prefix..after.len()-suffix].concat()}))
}

#[cfg(test)]
#[path = "delta_tests.rs"]
mod tests;
