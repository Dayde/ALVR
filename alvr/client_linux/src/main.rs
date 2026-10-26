// Desktop Linux client: runs as a regular OpenXR application on a local runtime (Monado) that
// drives the headset, and receives the stream from an ALVR streamer over the network.

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        mem,
        sync::atomic::{AtomicBool, Ordering},
    };

    const NO_WAYLAND_DISPLAY: &str = "alvr-no-wayland";

    static EXIT_SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_exit_signal(_: libc::c_int) {
        if EXIT_SIGNAL_RECEIVED.swap(true, Ordering::Relaxed) {
            // Second signal: the frame loop is stuck (for example in xrWaitFrame), give up.
            // _exit() is async-signal-safe, unlike std::process::exit().
            unsafe { libc::_exit(1) };
        }

        alvr_client_openxr::request_exit();
    }

    pub fn main() {
        // End the OpenXR session cleanly on SIGTERM (service stop, shutdown), SIGINT (Ctrl+C) and
        // SIGHUP (terminal closed), so that the runtime and the streamer see a normal disconnect.
        // SA_RESTART resumes the system calls interrupted by the signal instead of failing them.
        unsafe {
            let mut action: libc::sigaction = mem::zeroed();
            action.sa_sigaction = on_exit_signal as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);

            for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
                // Logging is not set up yet, entry_point() does it
                if libc::sigaction(signal, &action, std::ptr::null_mut()) == -1 {
                    eprintln!(
                        "Cannot handle signal {signal}, it will not end the session cleanly: {}",
                        std::io::Error::last_os_error()
                    );
                }
            }
        }

        // The client never opens a window. Hide the window system from wgpu's EGL setup so that
        // it uses the surfaceless platform (EGL_MESA_platform_surfaceless), which supports the
        // offscreen pbuffer the renderer needs (Mesa's Wayland platform doesn't) and doesn't
        // depend on a desktop session. libwayland connects to "wayland-0" when WAYLAND_DISPLAY
        // is unset, so it is pointed to a socket that doesn't exist instead of being removed.
        // Safety: no other thread exists yet.
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", NO_WAYLAND_DISPLAY);
            std::env::remove_var("DISPLAY");
        }

        if let Err(e) = alvr_client_openxr::entry_point() {
            alvr_common::error!("{e:#}");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("alvr_client_linux only runs on Linux");
    std::process::exit(1);
}
