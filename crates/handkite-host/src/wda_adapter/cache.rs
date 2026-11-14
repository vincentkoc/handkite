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

/// A hint for preparing normal input, never the final native unlock guard.
#[derive(Default)]
pub(super) struct Cache {
    native: RefCell<Option<NativeHint>>,
}

impl Cache {
    pub fn clear(&self) {
        self.native.replace(None);
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
        task.abort();
        other.abort();
    }
}
