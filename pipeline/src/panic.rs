//! Any panic stops the whole process at once (`docs/PIPELINE.md` 2.8;
//! `docs/DECISIONS.md` D-030).
//!
//! **Why.** Rust's default on a panic is to end only the panicking thread. In the pipeline
//! that would be dangerous: if the journal writer died, the watermark would freeze and the
//! run would hang; if the core died in the middle of a command, the rest of the pipeline
//! would carry on without it. So [`install_abort_on_panic`] installs a panic hook that
//! prints the panic, the thread's name and the `seq` it was working on, and then calls
//! `std::process::abort()`; recovery (11.8) takes over at the next start.
//!
//! **What a panic can leave released.** The other threads run on while the hook prints, so
//! the gate may release more of what is complete and durable, but none of the panicking
//! command: the gate releases a command's events only with its trailer, which a command
//! that panicked never writes (`gate.rs`). The one exception is a command that had already
//! emitted more events than the event ring holds (131,072): its first events streamed
//! through before the panic (2.5, 2.8; review finding F-PARTIAL-RELEASE). A fix for such a
//! poison pill must then reproduce the events already released (12.4).
//!
//! **Why a hook and not `panic = "abort"`.** The profile setting would cover release builds
//! only; the hook also covers tests and debug builds.
//!
//! **The current seq.** The core and replay record the `seq` they are applying in a
//! thread-local ([`set_current_seq`]); replay also records the decoded command
//! ([`set_current_command`]), so that a poison pill (a journaled command that makes the
//! engine panic, 2.8) is named in the message: its `seq` and what it was.
//!
//! **Printing.** The hook writes straight to standard error, not through `eprintln!`. The
//! test harness captures `eprintln!` output and prints it only when a test finishes, which
//! an aborted test never does, so the message would be lost.
//!
//! **A consequence for tests.** The hook is process-wide and stays installed. A test binary
//! that installs it (by starting a pipeline) can't also hold `#[should_panic]` tests: their
//! expected panics would abort it. Such tests belong in another binary.

use std::backtrace::{Backtrace, BacktraceStatus};
use std::cell::Cell;
use std::io::Write;
use std::panic::PanicHookInfo;
use std::sync::Once;

use engine::command::Command;

thread_local! {
    /// The `seq` this thread is applying; 0 (never a seq) when none.
    static CURRENT_SEQ: Cell<u64> = const { Cell::new(0) };
    /// The command this thread is applying, when replay keeps it.
    static CURRENT_COMMAND: Cell<Option<Command>> = const { Cell::new(None) };
}

/// Records the `seq` this thread is working on, for the panic message. One thread-local
/// store: cheap enough for the core to do per command.
pub fn set_current_seq(seq: u64) {
    CURRENT_SEQ.set(seq);
}

/// Records the command this thread is applying, for the panic message (replay only).
pub fn set_current_command(command: Option<Command>) {
    CURRENT_COMMAND.set(command);
}

/// The `seq` this thread recorded last; 0 if none.
pub fn current_seq() -> u64 {
    CURRENT_SEQ.get()
}

/// Installs the abort-on-panic hook, once per process; later calls do nothing.
pub fn install_abort_on_panic() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| std::panic::set_hook(Box::new(abort_on_panic)));
}

/// The hook: print what happened, then abort the process.
fn abort_on_panic(info: &PanicHookInfo<'_>) {
    // The lock keeps the lines together if two threads panic at once.
    let mut stderr = std::io::stderr().lock();
    // Errors writing to stderr are ignored: there is nowhere else to report them, and the
    // process aborts either way.
    let _ = writeln!(stderr, "{}", describe(info));
    let backtrace = Backtrace::capture();
    if backtrace.status() == BacktraceStatus::Captured {
        let _ = writeln!(stderr, "{backtrace}");
    }
    let _ = writeln!(stderr, "aborting the process: a pipeline thread panicked (docs/PIPELINE.md 2.8)");
    let _ = stderr.flush();
    std::process::abort();
}

/// The message: the thread, the panic and where, and the seq and command if recorded.
fn describe(info: &PanicHookInfo<'_>) -> String {
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("<unnamed>");
    let mut message = format!("thread '{name}' {info}");
    // `try_with`: a panic while the thread-locals are being torn down must not panic again.
    let seq = CURRENT_SEQ.try_with(Cell::get).unwrap_or(0);
    if seq != 0 {
        message.push_str(&format!("\n  while applying seq {seq}"));
    }
    if let Some(command) = CURRENT_COMMAND.try_with(Cell::get).ok().flatten() {
        message.push_str(&format!("\n  command: {command:?}"));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::command::SetMark;
    use engine::types::{MarketId, Price};
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command as Process;

    /// Set only in the child process that [`a_panic_in_any_thread_aborts_the_process`]
    /// starts.
    const CHILD: &str = "PIPELINE_PANIC_TEST_CHILD";

    /// Does nothing in a normal test run. In the child process it installs the hook and
    /// panics in a named thread that has recorded a seq and a command.
    #[test]
    fn child_that_panics() {
        if std::env::var_os(CHILD).is_none() {
            return;
        }
        install_abort_on_panic();
        install_abort_on_panic(); // a second call must not stack a second hook
        let worker = std::thread::Builder::new().name("core".into()).spawn(|| {
            set_current_seq(4_242);
            set_current_command(Some(Command::SetMark(SetMark {
                price: Price::new(103_001),
                market: MarketId::new(3),
            })));
            panic!("the engine found a crossed book");
        });
        let _ = worker.expect("spawned").join();
        // Never reached: the hook aborts the process inside the worker's panic.
        std::process::exit(0);
    }

    #[test]
    fn a_panic_in_any_thread_aborts_the_process() {
        let output = Process::new(std::env::current_exe().expect("the test binary"))
            .args(["panic::tests::child_that_panics", "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("RUST_BACKTRACE", "0")
            .output()
            .expect("the test binary runs");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.signal(), Some(libc::SIGABRT), "stderr: {stderr}");
        assert!(stderr.contains("thread 'core' panicked at"), "stderr: {stderr}");
        assert!(stderr.contains("the engine found a crossed book"), "stderr: {stderr}");
        assert!(stderr.contains("while applying seq 4242"), "stderr: {stderr}");
        assert!(
            stderr.contains("command: SetMark(SetMark { price: 103001, market: 3 })"),
            "stderr: {stderr}"
        );
        assert!(stderr.contains("aborting the process"), "stderr: {stderr}");
    }

    #[test]
    fn the_current_seq_is_per_thread() {
        set_current_seq(7);
        let other = std::thread::spawn(|| {
            let before = current_seq();
            set_current_seq(9);
            (before, current_seq())
        })
        .join()
        .expect("the thread ends cleanly");
        assert_eq!(other, (0, 9));
        assert_eq!(current_seq(), 7);
        set_current_seq(0);
    }
}
