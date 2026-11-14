use handkite_core::controller::RawObservation;
use handkite_core::protocol::{HardwareId, LockState};
use std::cell::RefCell;
use std::time::{Duration, Instant};

#[derive(Clone, Eq, PartialEq)]
pub(super) struct Key {
    pub device: HardwareId,
    pub session: String,
    pub runner: tokio::task::Id,
}

struct NativeHint {
    key: Key,
    observed: Instant,
}

pub(super) struct Full {
    key: Key,
    pub raw: RawObservation,
    pub captured_at: Instant,
    pub reported: Option<bool>,
}

impl Full {
    pub fn matches(
        &self,
        image: &handkite_core::protocol::ScreenImage,
        app: &str,
        reported: Option<bool>,
        now: Instant,
    ) -> bool {
        self.context_matches(app, reported, now) && self.raw.screenshot == *image
    }

    pub fn context_matches(&self, app: &str, reported: Option<bool>, now: Instant) -> bool {
        self.fresh(now)
            && self.raw.state.foreground_app.as_deref() == Some(app)
            && self.reported == reported
            && reported == Some(false)
    }

    pub(super) fn fresh(&self, now: Instant) -> bool {
        now.checked_duration_since(self.captured_at)
            .is_some_and(|age| age < Duration::from_secs(2))
    }
}

/// A hint for preparing normal input, never the final native unlock guard.
#[derive(Default)]
pub(super) struct Cache {
    native: RefCell<Option<NativeHint>>,
    full: RefCell<Option<Full>>,
}

impl Cache {
    pub fn clear(&self) {
        self.native.replace(None);
        self.clear_full();
    }

    pub fn clear_full(&self) {
        self.full.replace(None);
    }

    pub fn record_full(
        &self,
        key: Option<Key>,
        raw: &RawObservation,
        captured_at: Instant,
        reported: Option<bool>,
    ) {
        let key = key.filter(|_| super::normal_hint_context(raw, reported));
        self.full.replace(key.map(|key| Full {
            key,
            raw: raw.clone(),
            captured_at,
            reported,
        }));
    }

    pub fn take_full(&self, key: Option<&Key>, now: Instant) -> Option<Full> {
        self.full
            .take()
            .filter(|full| key == Some(&full.key) && full.fresh(now))
    }

    pub fn native(&self, key: Option<&Key>, now: Instant) -> Option<LockState> {
        let mut slot = self.native.borrow_mut();
        if slot.as_ref().is_some_and(|hint| {
            key == Some(&hint.key)
                && now
                    .checked_duration_since(hint.observed)
                    .is_some_and(|age| age < Duration::from_secs(30))
        }) {
            Some(LockState::Unlocked)
        } else {
            *slot = None;
            None
        }
    }

    pub fn record_native(&self, key: Option<Key>, state: LockState, now: Instant) {
        self.native.replace(
            key.filter(|_| state == LockState::Unlocked)
                .map(|key| NativeHint { key, observed: now }),
        );
    }

    pub fn refresh_native(
        &self,
        key: Option<Key>,
        now: Instant,
        query: impl FnOnce() -> LockState,
    ) -> LockState {
        let state = query();
        self.record_native(key, state, now);
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hints_expire_without_sliding_and_bind_all_owners() {
        let task = tokio::spawn(std::future::pending::<()>());
        let other = tokio::spawn(std::future::pending::<()>());
        let key = Key {
            device: HardwareId::new("fixture").unwrap(),
            session: "first".into(),
            runner: task.id(),
        };
        let now = Instant::now();
        let cache = Cache::default();
        cache.record_native(Some(key.clone()), LockState::Unlocked, now);
        assert_eq!(
            cache.native(Some(&key), now + Duration::from_secs(29)),
            Some(LockState::Unlocked)
        );
        assert_eq!(
            cache.native(Some(&key), now + Duration::from_secs(30)),
            None
        );
        for changed in [
            Key {
                session: "second".into(),
                ..key.clone()
            },
            Key {
                device: HardwareId::new("other").unwrap(),
                ..key.clone()
            },
            Key {
                runner: other.id(),
                ..key.clone()
            },
        ] {
            cache.record_native(Some(key.clone()), LockState::Unlocked, now);
            assert_eq!(cache.native(Some(&changed), now), None);
            assert_eq!(cache.native(Some(&key), now), None);
        }
        for state in [LockState::Locked, LockState::Uncertain] {
            cache.record_native(Some(key.clone()), LockState::Unlocked, now);
            let queried = std::cell::Cell::new(false);
            assert_eq!(
                cache.refresh_native(Some(key.clone()), now, || {
                    queried.set(true);
                    state
                }),
                state
            );
            assert!(queried.get());
            assert_eq!(cache.native(Some(&key), now), None);
        }
        cache.record_native(Some(key.clone()), LockState::Unlocked, now);
        cache.clear();
        assert_eq!(cache.native(Some(&key), now), None);

        let raw = RawObservation {
            connected_usb: key.device.clone(), wda_target: key.device.clone(),
            window: handkite_core::protocol::ObservationWindow {
                started_unix_ms: 10, finished_unix_ms: 11 },
            state: handkite_core::protocol::DeviceState { lock: LockState::Unlocked,
                wda_ready: true, foreground_app: Some("example.app".into()) },
            accessibility_tree: r#"<AppiumAUT><XCUIElementTypeApplication x="0" y="0" width="393" height="852"/></AppiumAUT>"#.into(),
            screenshot: handkite_core::protocol::ScreenImage {
                format: handkite_core::protocol::ImageFormat::Png, bytes: vec![1] },
        };
        cache.record_full(Some(key.clone()), &raw, now, Some(false));
        let previous = cache
            .take_full(Some(&key), now + Duration::from_secs(1))
            .unwrap();
        assert_eq!(previous.raw.window.finished_unix_ms, 11);
        assert_eq!(previous.captured_at, now);
        assert!(previous.matches(&raw.screenshot, "example.app", Some(false), now));
        assert!(!previous.matches(&raw.screenshot, "changed.app", Some(false), now));
        assert!(!previous.matches(&raw.screenshot, "example.app", None, now));
        assert!(!previous.matches(&raw.screenshot, "example.app", Some(true), now));
        let mut changed = raw.screenshot.clone();
        changed.bytes.push(2);
        assert!(!previous.matches(&changed, "example.app", Some(false), now));
        assert!(previous.context_matches("example.app", Some(false), now));
        assert!(!previous.context_matches("changed.app", Some(false), now));
        assert!(!previous.context_matches("example.app", None, now));
        assert!(!previous.context_matches("example.app", Some(true), now));
        assert!(!previous.context_matches(
            "example.app",
            Some(false),
            now + Duration::from_secs(2)
        ));
        assert!(!previous.matches(
            &raw.screenshot,
            "example.app",
            Some(false),
            now + Duration::from_secs(2)
        ));
        assert!(cache.take_full(Some(&key), now).is_none());
        cache.record_full(Some(key.clone()), &raw, now, Some(false));
        assert!(
            cache
                .take_full(Some(&key), now + Duration::from_secs(2))
                .is_none()
        );
        cache.record_full(None, &raw, now, Some(false));
        assert!(cache.take_full(Some(&key), now).is_none());
        let mut protected = raw.clone();
        protected.accessibility_tree = "<XCUIElementTypeSecureTextField/>".into();
        cache.record_full(Some(key.clone()), &protected, now, Some(false));
        assert!(cache.take_full(Some(&key), now).is_none());
        cache.record_full(Some(key.clone()), &raw, now, Some(false));
        assert!(
            cache
                .take_full(
                    Some(&Key {
                        session: "new".into(),
                        ..key.clone()
                    }),
                    now
                )
                .is_none()
        );
        task.abort();
        other.abort();
    }
}
