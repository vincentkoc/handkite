//! Opt-in human-like input pacing.
//!
//! `off` (the default) is a strict no-op: no waits, no randomness consumed, no
//! geometry change and no wire field. Other policies add seeded, bounded jitter:
//! - the MCP session waits a log-normal gap between actions and caps actions per
//!   minute, then moves coordinate taps inside the smallest known element frame
//!   and varies long-press and swipe durations. The daemon admits the resulting
//!   action exactly as it admits any caller-supplied action;
//! - the daemon shapes only the WDA motion of an already admitted action: press
//!   duration, an eased, slightly curved swipe path with the admitted endpoints
//!   and total duration, and per-key typing cadence with bursts.
//!
//! Pacing never retries, splits, or reorders input, and never changes revision,
//! admission, or verification checks.

use handkite_core::protocol::{Action, Motion, MotionPolicy, Point};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    #[default]
    Off,
    Natural,
    Cautious,
}

impl Policy {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "natural" => Some(Self::Natural),
            "cautious" => Some(Self::Cautious),
            _ => None,
        }
    }

    pub fn profile(self) -> Option<&'static Profile> {
        self.motion().map(profile)
    }

    /// The daemon-side motion policy; off has none.
    pub fn motion(self) -> Option<MotionPolicy> {
        match self {
            Self::Off => None,
            Self::Natural => Some(MotionPolicy::Natural),
            Self::Cautious => Some(MotionPolicy::Cautious),
        }
    }
}

pub fn profile(policy: MotionPolicy) -> &'static Profile {
    match policy {
        MotionPolicy::Natural => &NATURAL,
        MotionPolicy::Cautious => &CAUTIOUS,
    }
}

/// Per-call MCP override. Omitted fields fall back to the session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    #[serde(default)]
    pub policy: Option<Policy>,
    #[serde(default)]
    pub seed: Option<u64>,
}

/// Malformed arguments or `pacing` override.
#[derive(Debug, Eq, PartialEq)]
pub struct InvalidOverride;

impl Override {
    /// Remove and validate the optional `pacing` argument.
    pub fn take(arguments: &mut Value) -> Result<Option<Self>, InvalidOverride> {
        arguments
            .as_object_mut()
            .ok_or(InvalidOverride)?
            .remove("pacing")
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| InvalidOverride)
    }
}

pub fn override_schema() -> Value {
    json!({"type":"object","additionalProperties":false,
    "description":"Optional per-call human-like pacing override. Omit to use the session policy (default off: no added latency).",
    "properties":{
        "policy":{"type":"string","enum":["off","natural","cautious"]},
        "seed":{"type":"integer","minimum":0,"maximum":u64::MAX,
            "description":"Replays the same jitter for this call."}
    }})
}

/// Sent to the daemon with a paced `act` request; never sent when off, and
/// unrepresentable as off.
pub type Shape = Motion;

/// Bounds for one policy. Every sample is clamped into its range.
#[derive(Debug)]
pub struct Profile {
    /// Inter-action gap: log-normal median, sigma, and clamp, in ms.
    pub gap_median_ms: f64,
    pub gap_sigma: f64,
    pub gap_ms: (f64, f64),
    pub actions_per_minute: usize,
    /// Tap press duration.
    pub press_median_ms: f64,
    pub press_sigma: f64,
    pub press_ms: (f64, f64),
    pub double_tap_gap_ms: (f64, f64),
    /// Relative long-press duration spread.
    pub long_press_spread: f64,
    /// Swipe duration factor clamp, sampled log-normally around 1.
    pub swipe_factor: (f64, f64),
    pub swipe_press_ms: (f64, f64),
    /// Perpendicular bulge as a fraction of distance, capped in points.
    pub curve_fraction: (f64, f64),
    pub curve_max_pt: f64,
    pub swipe_segments: u32,
    /// Tap jitter: sigma as a fraction of frame size, maximum shift, and inset.
    pub tap_spread: f64,
    pub tap_max_shift_pt: f64,
    pub inset_fraction: f64,
    pub inset_min_pt: f64,
    /// Typing cadence.
    pub key_median_ms: f64,
    pub key_sigma: f64,
    pub key_ms: (f64, f64),
    pub key_hold_ms: (f64, f64),
    pub burst_len: (u32, u32),
    pub burst_pause_median_ms: f64,
    pub burst_pause_ms: (f64, f64),
    /// Whole-string fallback frequency for text the key path does not shape.
    pub text_frequency_median: f64,
    pub text_frequency: (f64, f64),
}

pub static NATURAL: Profile = Profile {
    gap_median_ms: 420.0,
    gap_sigma: 0.5,
    gap_ms: (140.0, 2_400.0),
    actions_per_minute: 40,
    press_median_ms: 95.0,
    press_sigma: 0.22,
    press_ms: (55.0, 170.0),
    double_tap_gap_ms: (70.0, 170.0),
    long_press_spread: 0.10,
    swipe_factor: (0.82, 1.22),
    swipe_press_ms: (35.0, 80.0),
    curve_fraction: (0.02, 0.07),
    curve_max_pt: 16.0,
    swipe_segments: 10,
    tap_spread: 0.15,
    tap_max_shift_pt: 6.0,
    inset_fraction: 0.15,
    inset_min_pt: 1.5,
    key_median_ms: 85.0,
    key_sigma: 0.35,
    key_ms: (35.0, 260.0),
    key_hold_ms: (28.0, 70.0),
    burst_len: (3, 7),
    burst_pause_median_ms: 260.0,
    burst_pause_ms: (120.0, 900.0),
    text_frequency_median: 210.0,
    text_frequency: (120.0, 330.0),
};

pub static CAUTIOUS: Profile = Profile {
    gap_median_ms: 900.0,
    gap_sigma: 0.45,
    gap_ms: (350.0, 4_500.0),
    actions_per_minute: 24,
    press_median_ms: 115.0,
    press_sigma: 0.2,
    press_ms: (70.0, 200.0),
    double_tap_gap_ms: (85.0, 190.0),
    long_press_spread: 0.15,
    swipe_factor: (0.8, 1.3),
    swipe_press_ms: (45.0, 100.0),
    curve_fraction: (0.015, 0.05),
    curve_max_pt: 12.0,
    swipe_segments: 12,
    tap_spread: 0.12,
    tap_max_shift_pt: 4.0,
    inset_fraction: 0.2,
    inset_min_pt: 2.0,
    key_median_ms: 130.0,
    key_sigma: 0.35,
    key_ms: (60.0, 340.0),
    key_hold_ms: (35.0, 85.0),
    burst_len: (2, 5),
    burst_pause_median_ms: 420.0,
    burst_pause_ms: (180.0, 1_400.0),
    text_frequency_median: 150.0,
    text_frequency: (80.0, 240.0),
};

/// Longest planned key sequence; above it the whole string is sent at once.
pub const MAX_SHAPED_TEXT_MS: u64 = 8_000;
pub const MAX_SHAPED_TEXT_CHARS: usize = 96;
const WINDOW: Duration = Duration::from_secs(60);

/// SplitMix64: small, seedable and stable across platforms and releases.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn uniform(&mut self, (low, high): (f64, f64)) -> f64 {
        low + (high - low) * self.unit()
    }

    pub fn normal(&mut self) -> f64 {
        let radius = (-2.0 * (1.0 - self.unit()).ln()).sqrt();
        radius * (std::f64::consts::TAU * self.unit()).cos()
    }

    pub fn log_normal(&mut self, median: f64, sigma: f64, (low, high): (f64, f64)) -> f64 {
        (median * (sigma * self.normal()).exp()).clamp(low, high)
    }

    fn count(&mut self, (low, high): (u32, u32)) -> u32 {
        low + (self.next_u64() % u64::from(high - low + 1)) as u32
    }
}

fn ms(value: f64) -> u64 {
    value.round().max(0.0) as u64
}

/// Session pacing state held by one MCP process.
pub struct Pacer {
    policy: Policy,
    rng: Rng,
    last: Option<Instant>,
    recent: VecDeque<Instant>,
    sleep: fn(Duration),
}

impl Default for Pacer {
    fn default() -> Self {
        Self::new(Policy::Off, 0)
    }
}

/// What one paced call resolved to.
pub struct Paced {
    pub policy: Policy,
    rng: Option<Rng>,
}

impl Paced {
    /// Shape for the daemon, only for actions whose WDA motion it shapes.
    pub fn shape_for(&mut self, action: &Action) -> Option<Shape> {
        let rng = self.rng.as_mut()?;
        let policy = self.policy.motion()?;
        if !matches!(
            action,
            Action::Tap { .. }
                | Action::DoubleTap { .. }
                | Action::Swipe { .. }
                | Action::Text { .. }
                | Action::TextCurrent { .. }
        ) {
            return None;
        }
        Some(Shape {
            policy,
            seed: rng.next_u64(),
        })
    }

    /// Move coordinates inside a known frame and vary hold durations.
    pub fn shape_action(&mut self, action: Action, frames: &[[f64; 4]]) -> Action {
        let (Some(rng), Some(profile)) = (self.rng.as_mut(), self.policy.profile()) else {
            return action;
        };
        match action {
            Action::Tap { at } => Action::Tap {
                at: jitter_tap(rng, profile, at, frames),
            },
            Action::DoubleTap { at } => Action::DoubleTap {
                at: jitter_tap(rng, profile, at, frames),
            },
            Action::LongPress { at, duration_ms } => Action::LongPress {
                at: jitter_tap(rng, profile, at, frames),
                duration_ms: spread_duration(rng, duration_ms, profile.long_press_spread),
            },
            Action::Swipe {
                from,
                to,
                duration_ms,
            } => {
                let factor = rng.log_normal(1.0, 0.12, profile.swipe_factor);
                Action::Swipe {
                    from,
                    to,
                    duration_ms: (f64::from(duration_ms) * factor)
                        .round()
                        .clamp(1.0, 5_000.0) as u32,
                }
            }
            other => other,
        }
    }
}

impl Pacer {
    pub fn new(policy: Policy, seed: u64) -> Self {
        Self {
            policy,
            rng: Rng::new(seed),
            last: None,
            recent: VecDeque::new(),
            sleep: std::thread::sleep,
        }
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    #[cfg(test)]
    pub(crate) fn without_sleep(mut self) -> Self {
        self.sleep = |_| {};
        self
    }

    /// Resolve the policy, wait as required, and record the action time.
    /// Off returns at once without consuming randomness.
    pub fn begin(&mut self, call: Option<Override>) -> Paced {
        let policy = call.and_then(|call| call.policy).unwrap_or(self.policy);
        if policy == Policy::Off {
            self.record(Instant::now());
            return Paced { policy, rng: None };
        }
        let mut rng = match call.and_then(|call| call.seed) {
            Some(seed) => Rng::new(seed),
            None => Rng::new(self.rng.next_u64()),
        };
        let wait = self.wait_before(policy, &mut rng, Instant::now());
        if !wait.is_zero() {
            (self.sleep)(wait);
        }
        self.record(Instant::now());
        Paced {
            policy,
            rng: Some(rng),
        }
    }

    /// Wait before the next action: the sampled gap minus time already spent
    /// since the previous action, and the per-minute cap.
    pub fn wait_before(&mut self, policy: Policy, rng: &mut Rng, now: Instant) -> Duration {
        let Some(profile) = policy.profile() else {
            return Duration::ZERO;
        };
        while self
            .recent
            .front()
            .is_some_and(|oldest| now.saturating_duration_since(*oldest) >= WINDOW)
        {
            self.recent.pop_front();
        }
        let gap = Duration::from_millis(ms(rng.log_normal(
            profile.gap_median_ms,
            profile.gap_sigma,
            profile.gap_ms,
        )));
        let gap_wait = self.last.map_or(Duration::ZERO, |last| {
            gap.saturating_sub(now.saturating_duration_since(last))
        });
        let cap = profile.actions_per_minute;
        let cap_wait = if self.recent.len() >= cap {
            let pivot = self.recent[self.recent.len() - cap];
            (pivot + WINDOW).saturating_duration_since(now)
        } else {
            Duration::ZERO
        };
        gap_wait.max(cap_wait)
    }

    pub fn record(&mut self, at: Instant) {
        self.last = Some(at);
        self.recent.push_back(at);
        let keep = CAUTIOUS.actions_per_minute.max(NATURAL.actions_per_minute);
        while self.recent.len() > keep {
            self.recent.pop_front();
        }
    }
}

fn spread_duration(rng: &mut Rng, duration_ms: u32, spread: f64) -> u32 {
    let factor = 1.0 + spread * rng.normal().clamp(-2.0, 2.0) / 2.0;
    (f64::from(duration_ms) * factor)
        .round()
        .clamp(1.0, 5_000.0) as u32
}

fn valid_frame([x, y, width, height]: [f64; 4]) -> bool {
    [x, y, width, height].into_iter().all(f64::is_finite) && width > 0.0 && height > 0.0
}

fn strictly_inside(at: Point, [x, y, width, height]: [f64; 4]) -> bool {
    at.x > x && at.x < x + width && at.y > y && at.y < y + height
}

/// Jitter `at` inside the smallest known frame that strictly contains it.
/// Without such a frame, or when the frame is too small, `at` is unchanged.
pub fn jitter_tap(rng: &mut Rng, profile: &Profile, at: Point, frames: &[[f64; 4]]) -> Point {
    let Some(frame) = frames
        .iter()
        .copied()
        .filter(|frame| valid_frame(*frame) && strictly_inside(at, *frame))
        .min_by(|a, b| (a[2] * a[3]).total_cmp(&(b[2] * b[3])))
    else {
        return at;
    };
    let [x, y, width, height] = frame;
    let inset = (profile.inset_fraction * width.min(height)).max(profile.inset_min_pt);
    let shift = profile.tap_max_shift_pt;
    // The admissible region: the inset frame, within the maximum shift of `at`.
    let left = (x + inset).max(at.x - shift);
    let right = (x + width - inset).min(at.x + shift);
    let top = (y + inset).max(at.y - shift);
    let bottom = (y + height - inset).min(at.y + shift);
    if left > right || top > bottom {
        return at;
    }
    let dx = rng.normal() * profile.tap_spread * width;
    let dy = rng.normal() * profile.tap_spread * height;
    let point = Point {
        x: (at.x + dx).clamp(left, right),
        y: (at.y + dy).clamp(top, bottom),
    };
    if strictly_inside(point, frame) {
        point
    } else {
        at
    }
}

// Daemon side: shape the WDA motion of the action being executed now.

fn point_valid(point: Point) -> bool {
    point.x.is_finite()
        && point.y.is_finite()
        && (0.0..=10_000.0).contains(&point.x)
        && (0.0..=10_000.0).contains(&point.y)
}

fn touch(steps: Vec<Value>) -> Value {
    json!({"actions": [{"type": "pointer", "id": "finger1", "parameters": {"pointerType": "touch"}, "actions": steps}]})
}

/// Pure: the same action and shape always produce the same request. `None`
/// keeps the unshaped payload, including for unsupported or invalid actions.
pub fn shaped_payload(action: &Action, shape: Shape) -> Option<(&'static str, Value)> {
    let profile = profile(shape.policy);
    let mut rng = Rng::new(shape.seed);
    match action {
        Action::Tap { at } if point_valid(*at) => {
            let press = ms(rng.log_normal(
                profile.press_median_ms,
                profile.press_sigma,
                profile.press_ms,
            ));
            Some((
                "/actions",
                touch(vec![
                    json!({"type":"pointerMove","duration":0,"x":at.x,"y":at.y}),
                    json!({"type":"pointerDown","button":0}),
                    json!({"type":"pause","duration":press}),
                    json!({"type":"pointerUp","button":0}),
                ]),
            ))
        }
        Action::DoubleTap { at } if point_valid(*at) => {
            let mut press = || {
                ms(rng.log_normal(
                    profile.press_median_ms * 0.85,
                    profile.press_sigma,
                    profile.press_ms,
                ))
            };
            let (first, second) = (press(), press());
            let gap = ms(rng.uniform(profile.double_tap_gap_ms));
            Some((
                "/actions",
                touch(vec![
                    json!({"type":"pointerMove","duration":0,"x":at.x,"y":at.y}),
                    json!({"type":"pointerDown","button":0}),
                    json!({"type":"pause","duration":first}),
                    json!({"type":"pointerUp","button":0}),
                    json!({"type":"pause","duration":gap}),
                    json!({"type":"pointerDown","button":0}),
                    json!({"type":"pause","duration":second}),
                    json!({"type":"pointerUp","button":0}),
                ]),
            ))
        }
        Action::Swipe {
            from,
            to,
            duration_ms,
        } if point_valid(*from) && point_valid(*to) && (1..=5_000).contains(duration_ms) => {
            let path = swipe_path(&mut rng, profile, *from, *to, *duration_ms)?;
            let press = ms(rng.uniform(profile.swipe_press_ms));
            let mut steps = vec![
                json!({"type":"pointerMove","duration":0,"x":from.x,"y":from.y,"origin":"viewport"}),
                json!({"type":"pointerDown","button":0}),
                json!({"type":"pause","duration":press}),
            ];
            steps.extend(path.into_iter().map(|(point, duration)| {
                json!({"type":"pointerMove","duration":duration,"x":point.x,"y":point.y,"origin":"viewport"})
            }));
            steps.push(json!({"type":"pointerUp","button":0}));
            Some(("/actions", touch(steps)))
        }
        Action::Text { value } | Action::TextCurrent { value }
            if handkite_core::protocol::injectable_text(value) =>
        {
            Some(match key_plan(&mut rng, profile, value) {
                Some(plan) => ("/actions", key_actions(&plan)),
                None => (
                    "/wda/keys",
                    json!({"value":[value],"frequency":ms(rng.log_normal(
                        profile.text_frequency_median, 0.2, profile.text_frequency))}),
                ),
            })
        }
        _ => None,
    }
}

fn ease_in_out(t: f64) -> f64 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Eased quadratic Bézier from `from` to `to`, returned as `(point, segment ms)`
/// after the start point. Durations sum to `duration_ms`; the last point is `to`.
pub fn swipe_path(
    rng: &mut Rng,
    profile: &Profile,
    from: Point,
    to: Point,
    duration_ms: u32,
) -> Option<Vec<(Point, u64)>> {
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    let distance = dx.hypot(dy);
    if distance < 1.0 {
        return None;
    }
    let sign = if rng.next_u64() & 1 == 0 { 1.0 } else { -1.0 };
    let bulge = (rng.uniform(profile.curve_fraction) * distance).min(profile.curve_max_pt) * sign;
    let (nx, ny) = (-dy / distance, dx / distance);
    // A quadratic Bézier peaks at half its control offset.
    let control = Point {
        x: (from.x + to.x) / 2.0 + nx * bulge * 2.0,
        y: (from.y + to.y) / 2.0 + ny * bulge * 2.0,
    };
    let segments = profile.swipe_segments.min(duration_ms / 8).max(1);
    let weights: Vec<f64> = (0..segments)
        .map(|_| rng.log_normal(1.0, 0.25, (0.5, 2.0)))
        .collect();
    let total: f64 = weights.iter().sum();
    let mut elapsed = 0u64;
    let mut fraction = 0.0;
    let mut path = Vec::with_capacity(segments as usize);
    for (index, weight) in weights.iter().enumerate() {
        fraction += weight / total;
        let last = index + 1 == segments as usize;
        let until = if last {
            u64::from(duration_ms)
        } else {
            ms(fraction * f64::from(duration_ms)).min(u64::from(duration_ms))
        };
        let point = if last {
            to
        } else {
            let s = ease_in_out(fraction.min(1.0));
            let inverse = 1.0 - s;
            Point {
                x: (inverse * inverse * from.x + 2.0 * inverse * s * control.x + s * s * to.x)
                    .clamp(0.0, 10_000.0),
                y: (inverse * inverse * from.y + 2.0 * inverse * s * control.y + s * s * to.y)
                    .clamp(0.0, 10_000.0),
            }
        };
        path.push((point, until.saturating_sub(elapsed)));
        elapsed = until.max(elapsed);
    }
    Some(path)
}

/// One planned key: the character, its hold, and the pause after it.
pub type Key = (char, u64, u64);

/// Per-key cadence with bursts, for short printable ASCII text only. Other
/// text keeps one whole-string request so no grapheme cluster is split.
pub fn key_plan(rng: &mut Rng, profile: &Profile, text: &str) -> Option<Vec<Key>> {
    if text.len() > MAX_SHAPED_TEXT_CHARS || !text.bytes().all(|byte| (0x20..0x7f).contains(&byte))
    {
        return None;
    }
    let mut plan = Vec::with_capacity(text.len());
    let mut burst = rng.count(profile.burst_len);
    let mut total = 0u64;
    let count = text.chars().count();
    for (index, character) in text.chars().enumerate() {
        let hold = ms(rng.uniform(profile.key_hold_ms));
        burst = burst.saturating_sub(1);
        let gap = if index + 1 == count {
            0
        } else if burst == 0 || (character == ' ' && rng.unit() < 0.5) {
            burst = rng.count(profile.burst_len);
            ms(rng.log_normal(profile.burst_pause_median_ms, 0.4, profile.burst_pause_ms))
        } else {
            ms(rng.log_normal(profile.key_median_ms, profile.key_sigma, profile.key_ms))
        };
        total += hold + gap;
        plan.push((character, hold, gap));
    }
    (total <= MAX_SHAPED_TEXT_MS).then_some(plan)
}

fn key_actions(plan: &[Key]) -> Value {
    let mut steps = Vec::with_capacity(plan.len() * 4);
    for (character, hold, gap) in plan {
        let value = character.to_string();
        steps.push(json!({"type":"keyDown","value":value}));
        steps.push(json!({"type":"pause","duration":hold}));
        steps.push(json!({"type":"keyUp","value":value}));
        if *gap > 0 {
            steps.push(json!({"type":"pause","duration":gap}));
        }
    }
    json!({"actions":[{"type":"key","id":"keyboard","actions":steps}]})
}
