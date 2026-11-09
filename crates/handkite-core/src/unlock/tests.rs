use super::*;

const CORRECT: [u8; 6] = [1, 2, 3, 4, 5, 6];

#[derive(Default)]
struct MemoryGate {
    disabled: bool,
    marks: usize,
    clears: usize,
}

impl AttemptGate for MemoryGate {
    fn is_disabled(&self) -> Result<bool, GateError> {
        Ok(self.disabled)
    }

    fn mark_uncertain(&mut self) -> Result<(), GateError> {
        self.disabled = true;
        self.marks += 1;
        Ok(())
    }

    fn clear(&mut self) -> Result<(), GateError> {
        self.disabled = false;
        self.clears += 1;
        Ok(())
    }
}

struct FakePhone {
    usb: HardwareId,
    wda_target: HardwareId,
    input: Vec<u8>,
    locked: bool,
    keypad: bool,
    entered_first: bool,
    stale_wda: bool,
    ambiguous: bool,
    bfu: bool,
    trust: bool,
    disconnected: bool,
    drop_at: Option<usize>,
    delay_at: Option<usize>,
    pending_digit: Option<u8>,
    double_at: Option<usize>,
    disconnect_at: Option<usize>,
    early_unlock_at: Option<usize>,
    presses: usize,
    failed_attempts: usize,
}

impl FakePhone {
    fn new() -> Self {
        let id = HardwareId::new("fixture-phone-a").unwrap();
        Self {
            usb: id.clone(),
            wda_target: id,
            input: Vec::new(),
            locked: true,
            keypad: false,
            entered_first: false,
            stale_wda: false,
            ambiguous: false,
            bfu: false,
            trust: false,
            disconnected: false,
            drop_at: None,
            delay_at: None,
            pending_digit: None,
            double_at: None,
            disconnect_at: None,
            early_unlock_at: None,
            presses: 0,
            failed_attempts: 0,
        }
    }

    fn accept(&mut self, digit: u8) {
        self.input.push(digit);
        if self.input.len() == CORRECT.len() {
            if self.input == CORRECT {
                self.locked = false;
            } else {
                self.failed_attempts += 1;
                self.input.clear();
            }
        }
    }
}

impl PasscodeBackend for FakePhone {
    fn snapshot(&mut self) -> Result<PasscodeScreen, BackendError> {
        if self.disconnected {
            return Err(BackendError::Disconnected);
        }
        let visible_lock = if self.ambiguous {
            LockState::Uncertain
        } else if self.locked {
            LockState::Locked
        } else {
            LockState::Unlocked
        };
        let count = self.input.len();
        let secure_field_value = (self.keypad && self.locked).then(|| {
            if self.entered_first {
                format!("{count} of 6 digits")
            } else {
                format!("6-digit passcode, {count} entered")
            }
        });
        Ok(PasscodeScreen {
            connected_usb: self.usb.clone(),
            wda_target: self.wda_target.clone(),
            native_lock: if self.locked {
                LockState::Locked
            } else {
                LockState::Unlocked
            },
            visible_lock,
            wda_lock: if self.stale_wda || self.locked {
                LockState::Locked
            } else {
                LockState::Unlocked
            },
            keypad_ready: self.keypad,
            secure_field_value,
            before_first_unlock: self.bfu,
            trust_prompt: self.trust,
        })
    }

    fn show_keypad(&mut self) -> Result<(), BackendError> {
        self.keypad = true;
        Ok(())
    }

    fn press_digit(&mut self, digit: u8) -> Result<(), BackendError> {
        self.presses += 1;
        if self.drop_at == Some(self.presses) {
            return Ok(());
        }
        if self.delay_at == Some(self.presses) {
            self.pending_digit = Some(digit);
            return Ok(());
        }
        self.accept(digit);
        if self.double_at == Some(self.presses) {
            self.accept(digit);
        }
        if self.early_unlock_at == Some(self.presses) {
            self.locked = false;
        }
        if self.disconnect_at == Some(self.presses) {
            self.disconnected = true;
        }
        Ok(())
    }

    fn delete_digit(&mut self) -> Result<(), BackendError> {
        self.input.pop();
        Ok(())
    }
}

fn fixture() -> (DeviceBinding, DeviceKey, UnlockPolicy) {
    let key = DeviceKey::new("fixture-phone").unwrap();
    let binding = DeviceBinding {
        key: key.clone(),
        usb_id: HardwareId::new("fixture-phone-a").unwrap(),
    };
    let policy = UnlockPolicy {
        progress_polls: 0,
        final_polls: 0,
        poll_delay: Duration::ZERO,
    };
    (binding, key, policy)
}

fn run(
    phone: &mut FakePhone,
    gate: &mut MemoryGate,
    code: &str,
) -> Result<UnlockOutcome, UnlockError> {
    let (binding, key, policy) = fixture();
    unlock_once(
        phone,
        gate,
        &binding,
        &key,
        &Passcode::new(code.into()).unwrap(),
        policy,
    )
}

#[test]
fn submits_all_digits_with_either_count_order_and_stale_wda_flag() {
    for entered_first in [false, true] {
        let mut phone = FakePhone::new();
        phone.entered_first = entered_first;
        phone.stale_wda = true;
        let mut gate = MemoryGate::default();
        assert_eq!(
            run(&mut phone, &mut gate, "123456"),
            Ok(UnlockOutcome::CodeSubmitted)
        );
        assert_eq!(phone.presses, 6);
        assert_eq!(phone.failed_attempts, 0);
        assert!(!phone.locked);
        assert!(!gate.disabled);
        assert_eq!((gate.marks, gate.clears), (1, 1));
    }
}

#[test]
fn wrong_code_disables_the_next_attempt() {
    let mut phone = FakePhone::new();
    let mut gate = MemoryGate::default();
    assert_eq!(
        run(&mut phone, &mut gate, "123450"),
        Err(UnlockError::WrongOrUncertain)
    );
    assert_eq!(phone.failed_attempts, 1);
    assert!(gate.disabled);
    assert_eq!(
        run(&mut phone, &mut gate, "123456"),
        Err(UnlockError::Disabled)
    );
    assert_eq!(phone.failed_attempts, 1);
}

#[test]
fn dropped_and_doubled_digits_stop_before_a_submission() {
    for mode in 0..2 {
        let mut phone = FakePhone::new();
        if mode == 0 {
            phone.drop_at = Some(3)
        } else {
            phone.double_at = Some(2)
        };
        let mut gate = MemoryGate::default();
        assert!(matches!(
            run(&mut phone, &mut gate, "123456"),
            Err(UnlockError::Incomplete { .. })
        ));
        assert_eq!(phone.failed_attempts, 0);
        assert!(phone.presses < 6);
        assert!(gate.disabled);
        assert_eq!((gate.marks, gate.clears), (1, 0));
        let presses = phone.presses;
        assert_eq!(
            run(&mut phone, &mut gate, "123456"),
            Err(UnlockError::Disabled)
        );
        assert_eq!(phone.presses, presses);
    }
}

#[test]
fn doubled_digit_that_reaches_full_length_disables_the_next_attempt() {
    let mut phone = FakePhone::new();
    phone.double_at = Some(5);
    let mut gate = MemoryGate::default();
    assert_eq!(
        run(&mut phone, &mut gate, "123456"),
        Err(UnlockError::WrongOrUncertain)
    );
    assert_eq!(phone.failed_attempts, 1);
    assert!(gate.disabled);
    assert_eq!(phone.presses, 5);
}

#[test]
fn a_dropped_final_digit_is_incomplete() {
    let mut phone = FakePhone::new();
    phone.drop_at = Some(6);
    let mut gate = MemoryGate::default();
    assert_eq!(
        run(&mut phone, &mut gate, "123456"),
        Err(UnlockError::Incomplete { digit: 6 })
    );
    assert_eq!(phone.failed_attempts, 0);
    assert!(gate.disabled);
    assert_eq!((gate.marks, gate.clears), (1, 0));
    assert_eq!(
        run(&mut phone, &mut gate, "123456"),
        Err(UnlockError::Disabled)
    );
    assert_eq!(phone.presses, 6);
}

#[test]
fn residual_input_is_cleared_before_the_first_new_digit() {
    let mut phone = FakePhone::new();
    phone.keypad = true;
    phone.input = vec![9, 9];
    let mut gate = MemoryGate::default();
    assert_eq!(
        run(&mut phone, &mut gate, "123456"),
        Ok(UnlockOutcome::CodeSubmitted)
    );
    assert_eq!(phone.presses, 6);
}

#[test]
fn early_unlock_never_counts_as_full_code_submission() {
    let mut phone = FakePhone::new();
    phone.early_unlock_at = Some(2);
    let mut gate = MemoryGate::default();
    assert_eq!(
        run(&mut phone, &mut gate, "123456"),
        Err(UnlockError::UnlockedBeforeLastDigit)
    );
    assert_eq!(phone.presses, 2);
    assert!(gate.disabled);
    assert_eq!((gate.marks, gate.clears), (1, 0));
    phone.locked = true;
    phone.keypad = false;
    phone.input.clear();
    assert_eq!(
        run(&mut phone, &mut gate, "123456"),
        Err(UnlockError::Disabled)
    );
    assert_eq!(phone.presses, 2);
}

#[test]
fn delayed_digit_cannot_reopen_a_second_attempt() {
    for delayed_at in [3, 6] {
        let mut phone = FakePhone::new();
        phone.delay_at = Some(delayed_at);
        let mut gate = MemoryGate::default();
        assert!(matches!(
            run(&mut phone, &mut gate, "123456"),
            Err(UnlockError::Incomplete { .. })
        ));
        assert_eq!(phone.presses, delayed_at);
        let pending = phone.pending_digit.take().unwrap();
        phone.accept(pending);
        assert!(gate.disabled);
        assert_eq!((gate.marks, gate.clears), (1, 0));
        assert_eq!(
            run(&mut phone, &mut gate, "123456"),
            Err(UnlockError::Disabled)
        );
        assert_eq!(phone.presses, delayed_at);
    }
}

#[test]
fn ambiguous_screen_and_second_phone_block_before_input() {
    let mut phone = FakePhone::new();
    phone.ambiguous = true;
    let mut gate = MemoryGate::default();
    assert_eq!(
        run(&mut phone, &mut gate, "123456"),
        Err(UnlockError::ManualRequired)
    );
    assert_eq!(phone.presses, 0);

    phone.ambiguous = false;
    phone.usb = HardwareId::new("fixture-phone-b").unwrap();
    assert_eq!(
        run(&mut phone, &mut gate, "123456"),
        Err(UnlockError::DeviceMismatch)
    );
    assert_eq!(phone.presses, 0);
}

#[test]
fn first_unlock_and_trust_prompts_require_a_person() {
    for prompt in 0..2 {
        let mut phone = FakePhone::new();
        phone.bfu = prompt == 0;
        phone.trust = prompt == 1;
        let mut gate = MemoryGate::default();
        assert_eq!(
            run(&mut phone, &mut gate, "123456"),
            Err(UnlockError::ManualRequired)
        );
        assert_eq!(phone.presses, 0);
        assert!(!gate.disabled);
    }
}

#[test]
fn disconnect_after_input_leaves_the_gate_disabled() {
    let mut phone = FakePhone::new();
    phone.disconnect_at = Some(2);
    let mut gate = MemoryGate::default();
    assert_eq!(
        run(&mut phone, &mut gate, "123456"),
        Err(UnlockError::Backend(BackendError::Disconnected))
    );
    assert!(gate.disabled);
    assert_eq!(phone.presses, 2);
}

#[test]
fn stale_wda_flag_does_not_enter_a_code_on_an_unlocked_phone() {
    let mut phone = FakePhone::new();
    phone.locked = false;
    phone.stale_wda = true;
    let mut gate = MemoryGate::default();
    assert_eq!(
        run(&mut phone, &mut gate, "123456"),
        Ok(UnlockOutcome::AlreadyUnlocked)
    );
    assert_eq!(phone.presses, 0);
}
