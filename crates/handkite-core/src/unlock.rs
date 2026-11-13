use crate::passcode::{Passcode, PasscodeError, PasscodeProgress};
use crate::protocol::{DeviceBinding, DeviceKey, HardwareId, LockState};
use std::thread;
use std::time::Duration;

/// Evidence from the selected USB device and its active WDA session.
pub struct PasscodeScreen {
    pub connected_usb: HardwareId,
    pub wda_target: HardwareId,
    pub native_lock: LockState,
    pub visible_lock: LockState,
    pub wda_lock: LockState,
    pub keypad_ready: bool,
    pub secure_field_value: Option<String>,
    pub before_first_unlock: bool,
    pub trust_prompt: bool,
}

impl PasscodeScreen {
    fn confirmed_unlocked(&self) -> bool {
        self.native_lock == LockState::Unlocked && self.visible_lock == LockState::Unlocked
    }

    fn confirmed_locked(&self) -> bool {
        self.native_lock == LockState::Locked && self.visible_lock == LockState::Locked
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendError {
    Disconnected,
    Unavailable,
    ProgressMismatch,
}

pub trait PasscodeBackend {
    fn snapshot(&mut self) -> Result<PasscodeScreen, BackendError>;
    fn show_keypad(&mut self) -> Result<(), BackendError>;
    fn press_digit(&mut self, digit: u8) -> Result<(), BackendError>;
    fn delete_digit(&mut self) -> Result<(), BackendError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GateError;

/// The implementation must persist the marker before acknowledging `mark_uncertain`.
pub trait AttemptGate {
    fn is_disabled(&self) -> Result<bool, GateError>;
    fn mark_uncertain(&mut self) -> Result<(), GateError>;
    fn clear(&mut self) -> Result<(), GateError>;
}

#[derive(Clone, Copy, Debug)]
pub struct UnlockPolicy {
    pub progress_polls: u8,
    pub final_polls: u8,
    pub poll_delay: Duration,
}

impl Default for UnlockPolicy {
    fn default() -> Self {
        Self {
            progress_polls: 10,
            final_polls: 20,
            poll_delay: Duration::from_millis(150),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnlockOutcome {
    CodeSubmitted,
    AlreadyUnlocked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IncompleteProgress {
    Unchanged,
    Unexpected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnlockError {
    Disabled,
    DeviceMismatch,
    ManualRequired,
    NoKeypad,
    UnreadableProgress,
    LengthMismatch {
        observed_total: u32,
    },
    ResidualInput,
    Incomplete {
        digit: u32,
        progress: IncompleteProgress,
    },
    UnlockedBeforeLastDigit,
    WrongOrUncertain,
    Backend(BackendError),
    Gate,
}

impl From<BackendError> for UnlockError {
    fn from(error: BackendError) -> Self {
        Self::Backend(error)
    }
}

impl From<GateError> for UnlockError {
    fn from(_: GateError) -> Self {
        Self::Gate
    }
}

impl From<PasscodeError> for UnlockError {
    fn from(error: PasscodeError) -> Self {
        match error {
            PasscodeError::LengthMismatch { observed_total } => {
                Self::LengthMismatch { observed_total }
            }
            _ => Self::UnreadableProgress,
        }
    }
}

fn sample<B: PasscodeBackend>(
    backend: &mut B,
    binding: &DeviceBinding,
    requested: &DeviceKey,
) -> Result<PasscodeScreen, UnlockError> {
    let screen = backend.snapshot()?;
    binding
        .verify(requested, &screen.connected_usb, &screen.wda_target)
        .map_err(|_| UnlockError::DeviceMismatch)?;
    Ok(screen)
}

fn progress(
    screen: &PasscodeScreen,
    expected_total: u32,
) -> Result<Option<PasscodeProgress>, UnlockError> {
    screen
        .secure_field_value
        .as_deref()
        .map(|label| PasscodeProgress::parse(label, expected_total).map_err(UnlockError::from))
        .transpose()
}

fn wait_for_count<B: PasscodeBackend>(
    backend: &mut B,
    binding: &DeviceBinding,
    requested: &DeviceKey,
    total: u32,
    before: u32,
    policy: UnlockPolicy,
) -> Result<Step, UnlockError> {
    for poll in 0..=policy.progress_polls {
        let screen = sample(backend, binding, requested)?;
        if screen.confirmed_unlocked() {
            return Ok(Step::Unlocked);
        }
        if !screen.confirmed_locked() || screen.before_first_unlock || screen.trust_prompt {
            return Err(UnlockError::WrongOrUncertain);
        }
        if let Some(field) = progress(&screen, total)? {
            if field.entered != before {
                return Ok(Step::Count(field.entered));
            }
            if poll == policy.progress_polls {
                return Ok(Step::Count(before));
            }
        } else if poll == policy.progress_polls {
            return Err(UnlockError::WrongOrUncertain);
        }
        thread::sleep(policy.poll_delay);
    }
    unreachable!()
}

enum Step {
    Count(u32),
    Unlocked,
}

fn confirm_final<B: PasscodeBackend>(
    backend: &mut B,
    binding: &DeviceBinding,
    requested: &DeviceKey,
    total: u32,
    before: u32,
    policy: UnlockPolicy,
) -> Result<FinalState, UnlockError> {
    let mut last_count = None;
    for poll in 0..=policy.final_polls {
        let screen = sample(backend, binding, requested)?;
        if screen.confirmed_unlocked() {
            return Ok(FinalState::Unlocked);
        }
        if !screen.confirmed_locked() || screen.before_first_unlock || screen.trust_prompt {
            return Err(UnlockError::WrongOrUncertain);
        }
        last_count = progress(&screen, total)?.map(|field| field.entered);
        if poll < policy.final_polls {
            thread::sleep(policy.poll_delay);
        }
    }
    if last_count == Some(before) {
        Ok(FinalState::Incomplete)
    } else {
        Err(UnlockError::WrongOrUncertain)
    }
}

enum FinalState {
    Unlocked,
    Incomplete,
}

/// Submit one supplied code at most once. Uncertain field input leaves the gate disabled.
pub fn unlock_once<B: PasscodeBackend, G: AttemptGate>(
    backend: &mut B,
    gate: &mut G,
    binding: &DeviceBinding,
    requested: &DeviceKey,
    code: &Passcode,
    policy: UnlockPolicy,
) -> Result<UnlockOutcome, UnlockError> {
    if gate.is_disabled()? {
        return Err(UnlockError::Disabled);
    }
    let mut screen = sample(backend, binding, requested)?;
    if screen.confirmed_unlocked() {
        return Ok(UnlockOutcome::AlreadyUnlocked);
    }
    if !screen.confirmed_locked() || screen.before_first_unlock || screen.trust_prompt {
        return Err(UnlockError::ManualRequired);
    }

    backend.show_keypad()?;
    screen = sample(backend, binding, requested)?;
    if !screen.confirmed_locked() || screen.before_first_unlock || screen.trust_prompt {
        return Err(UnlockError::ManualRequired);
    }
    if !screen.keypad_ready {
        return Err(UnlockError::NoKeypad);
    }
    let mut field = progress(&screen, code.len())?.ok_or(UnlockError::UnreadableProgress)?;
    gate.mark_uncertain()?;
    while field.entered > 0 {
        backend.delete_digit()?;
        match wait_for_count(
            backend,
            binding,
            requested,
            code.len(),
            field.entered,
            policy,
        )? {
            Step::Count(next) if next + 1 == field.entered => field.entered = next,
            _ => return Err(UnlockError::ResidualInput),
        }
    }

    for index in 0..code.len() as usize {
        backend.press_digit(code.digit(index))?;
        let before = index as u32;
        if index + 1 == code.len() as usize {
            match confirm_final(backend, binding, requested, code.len(), before, policy)? {
                FinalState::Unlocked => {
                    gate.clear()?;
                    return Ok(UnlockOutcome::CodeSubmitted);
                }
                FinalState::Incomplete => {
                    return Err(UnlockError::Incomplete {
                        digit: index as u32 + 1,
                        progress: IncompleteProgress::Unchanged,
                    });
                }
            }
        }
        match wait_for_count(backend, binding, requested, code.len(), before, policy)? {
            Step::Count(next) if next == before + 1 => {}
            Step::Count(next) if next >= before && next < code.len() => {
                return Err(UnlockError::Incomplete {
                    digit: index as u32 + 1,
                    progress: if next == before {
                        IncompleteProgress::Unchanged
                    } else {
                        IncompleteProgress::Unexpected
                    },
                });
            }
            Step::Unlocked => {
                return Err(UnlockError::UnlockedBeforeLastDigit);
            }
            _ => return Err(UnlockError::WrongOrUncertain),
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests;
