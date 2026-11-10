use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn request_shutdown(_: libc::c_int) {
    REQUESTED.store(true, Ordering::Release);
}

pub(super) fn requested() -> bool {
    REQUESTED.load(Ordering::Acquire)
}

/// Keep both handlers installed until the service has dropped its device adapter.
pub(super) struct ShutdownSignals {
    previous_term: libc::sigaction,
    previous_int: libc::sigaction,
}

impl ShutdownSignals {
    pub(super) fn install() -> Result<Self, &'static str> {
        REQUESTED.store(false, Ordering::Release);
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = request_shutdown as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
            return Err("cannot install shutdown signals");
        }
        let mut previous_term = MaybeUninit::<libc::sigaction>::uninit();
        if unsafe { libc::sigaction(libc::SIGTERM, &action, previous_term.as_mut_ptr()) } != 0 {
            return Err("cannot install shutdown signals");
        }
        let previous_term = unsafe { previous_term.assume_init() };
        let mut previous_int = MaybeUninit::<libc::sigaction>::uninit();
        if unsafe { libc::sigaction(libc::SIGINT, &action, previous_int.as_mut_ptr()) } != 0 {
            unsafe { libc::sigaction(libc::SIGTERM, &previous_term, std::ptr::null_mut()) };
            return Err("cannot install shutdown signals");
        }
        Ok(Self {
            previous_term,
            previous_int: unsafe { previous_int.assume_init() },
        })
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        unsafe {
            libc::sigaction(libc::SIGINT, &self.previous_int, std::ptr::null_mut());
            libc::sigaction(libc::SIGTERM, &self.previous_term, std::ptr::null_mut());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "child entrypoint, invoked only by the parent signal test"]
    fn signal_child() {
        let _handlers = ShutdownSignals::install().unwrap();
        println!("signal-child-ready");
        std::io::stdout().flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !requested() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(requested());
    }

    #[test]
    fn signals_stop_only_the_owned_test_children() {
        for signal in [libc::SIGTERM, libc::SIGINT] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "signals::tests::signal_child",
                    "--ignored",
                    "--nocapture",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let stdout = child.stdout.take().unwrap();
            let (ready, received) = mpsc::channel();
            let reader = thread::spawn(move || {
                for line in std::io::BufReader::new(stdout)
                    .lines()
                    .map_while(Result::ok)
                {
                    if line.contains("signal-child-ready") {
                        let _ = ready.send(());
                    }
                }
            });
            if received.recv_timeout(Duration::from_secs(3)).is_err() {
                child.kill().unwrap();
                child.wait().unwrap();
                reader.join().unwrap();
                panic!("signal test child did not become ready");
            }
            assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, signal) }, 0);
            let deadline = Instant::now() + Duration::from_secs(3);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    reader.join().unwrap();
                    panic!("signal test child did not exit");
                }
                thread::sleep(Duration::from_millis(10));
            };
            reader.join().unwrap();
            assert!(status.success());
        }
    }
}
