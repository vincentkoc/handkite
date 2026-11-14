use super::response;
use crate::wire::WireObservation;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum TreeFormat {
    #[default]
    Full,
    Compact,
    Outline,
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ImageMode {
    #[default]
    Always,
    Omit,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Options {
    #[serde(default)]
    pub tree_format: TreeFormat,
    #[serde(default)]
    pub image: ImageMode,
}

pub(super) fn schema() -> Value {
    json!({"type":"object", "additionalProperties":false, "properties":{
        "tree_format":{"type":"string","enum":["full","compact","outline"],
            "description":"Response tree presentation only; internal action checks retain the full capture."},
        "image":{"type":"string","enum":["always","omit"],
            "description":"Include or omit the response image. Does not skip internal capture."}
    }})
}

pub(super) fn content(observation: WireObservation, options: Options) -> Vec<Value> {
    if options.tree_format == TreeFormat::Outline {
        return response::outlined_observation_content(
            observation,
            options.image == ImageMode::Always,
        );
    }
    match (options.tree_format, options.image) {
        (TreeFormat::Full, ImageMode::Always) => response::observation_content(observation),
        (TreeFormat::Compact, ImageMode::Always) => {
            response::compact_observation_content(observation)
        }
        (TreeFormat::Outline, _) => unreachable!(),
        (format, ImageMode::Omit) => response::selected_observation_content(
            observation,
            format == TreeFormat::Compact,
            false,
        ),
    }
}
