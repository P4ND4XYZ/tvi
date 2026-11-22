//! Standalone renderer substitute, compiled with rustc by the integration test.

use std::{env, fs, path::Path, process::Command, thread, time::Duration};

const RED: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0,
    0, 0, 144, 119, 83, 222, 0, 0, 0, 15, 73, 68, 65, 84, 120, 1, 1, 4, 0, 251, 255, 0, 255, 0, 0,
    3, 1, 1, 0, 141, 29, 229, 130, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];
const BLUE: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0,
    0, 0, 144, 119, 83, 222, 0, 0, 0, 15, 73, 68, 65, 84, 120, 1, 1, 4, 0, 251, 255, 0, 0, 0, 255,
    1, 3, 1, 0, 124, 194, 116, 219, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

fn main() {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args == ["--hold"] {
        hold("descendant-running", "descendant-ready");
        return;
    }
    #[cfg(unix)]
    if args == ["--ignore-term"] {
        unix::install();
        hold("descendant-running", "descendant-ready");
        return;
    }
    assert_eq!(
        args,
        ["-i", "input.mmd", "-o", "output.png", "--scale", "2"]
    );
    let marker = fs::read_to_string("input.mmd").unwrap();
    if marker.trim_end() == "record_launch" {
        fs::write("../renderer-launched", b"started").unwrap();
    }
    match marker.as_str() {
        "exit" => {
            eprintln!("private renderer data");
            std::process::exit(17);
        }
        "missing" => {}
        "invalid" => fs::write("output.png", &RED[..8]).unwrap(),
        "timeout" => {
            let mut child = Command::new(env::current_exe().unwrap())
                .arg("--hold")
                .spawn()
                .unwrap();
            hold("running", "ready");
            child.wait().unwrap();
        }
        #[cfg(unix)]
        "cooperative" | "unresponsive" => unix::simulate(marker == "cooperative"),
        "blue" => fs::write("output.png", BLUE).unwrap(),
        _ => fs::write("output.png", RED).unwrap(),
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::{
        ffi::c_int,
        os::unix::process::CommandExt,
        sync::atomic::{AtomicBool, Ordering},
        time::Instant,
    };

    type Handler = extern "C" fn(c_int);
    // This test installs only a valid function pointer, and ignores the previous handler.
    unsafe extern "C" {
        fn signal(number: c_int, handler: Handler) -> usize;
    }

    static TERM: AtomicBool = AtomicBool::new(false);

    extern "C" fn notice_term(_: c_int) {
        TERM.store(true, Ordering::Relaxed);
    }

    pub(super) fn install() {
        // SAFETY: the handler only performs a lock-free atomic store.
        assert_ne!(unsafe { signal(15, notice_term) }, usize::MAX);
    }

    pub(super) fn simulate(cooperate: bool) {
        install();
        let mut browser = Command::new(env::current_exe().unwrap());
        if cooperate {
            browser.arg("--hold").process_group(0);
        } else {
            browser.arg("--ignore-term");
        }
        let mut browser = browser.spawn().unwrap();
        let lock = fs::File::create("../running").unwrap();
        lock.lock().unwrap();
        fs::write("../ready", b"ready").unwrap();
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(4) {
            if cooperate && TERM.load(Ordering::Relaxed) {
                browser.kill().unwrap();
                browser.wait().unwrap();
                fs::write("../term-handled", b"clean").unwrap();
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        drop(browser.kill());
        drop(browser.wait());
    }
}

fn hold(lock_name: &str, ready_name: &str) {
    // A released OS lock proves termination without inspecting the process table.
    let lock = fs::File::create(Path::new("..").join(lock_name)).unwrap();
    lock.lock().unwrap();
    fs::write(Path::new("..").join(ready_name), b"ready").unwrap();
    // Also bound the lifetime of the fake if the termination code regresses.
    thread::sleep(Duration::from_secs(4));
    drop(lock);
}
