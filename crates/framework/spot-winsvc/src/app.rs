//! The `spot serve` side of the ordering state machine (brief: "`run` ... start
//! `spot-pg-supervisor` ... then `spot serve` as a child process ... with job-object
//! kill-on-close"). [`spot_cli`] exposes no `run_serve` entry point today (its public
//! surface is `host::{Options, Running, start, start_with, start_with_audit}`, consumed
//! by the `spot` binary's own `Serve` subcommand — see Open Questions), so this crate
//! runs `spot serve` as a child process rather than in-process.

use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::watch;

/// `spot serve`'s lifecycle as the orchestrator sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppLifecycle {
    /// Not running.
    Down,
    /// Started, not yet answering `/api/health/ready`.
    Starting,
    /// `/api/health/ready` answers.
    Ready,
    /// Exited on its own (a crash, as opposed to a requested stop).
    Exited {
        /// The process exit code, when the platform reports one.
        code: Option<i32>,
    },
}

/// What the orchestrator needs from the `spot serve` child: start it, watch it, stop it.
#[allow(async_fn_in_trait)]
pub trait AppControl: Send + Sync {
    /// Watches lifecycle transitions.
    fn subscribe(&self) -> watch::Receiver<AppLifecycle>;
    /// The lifecycle right now.
    fn state(&self) -> AppLifecycle;
    /// Spawns `spot serve`.
    async fn start(&self) -> Result<(), crate::error::WinsvcError>;
    /// Polls `/api/health/ready` until it answers or `timeout` elapses.
    async fn wait_ready(&self, timeout: Duration) -> Result<(), crate::error::WinsvcError>;
    /// Sends the stop signal, waits up to `grace`, then forces the stop.
    async fn stop(&self, grace: Duration) -> Result<(), crate::error::WinsvcError>;
}

/// Where the `spot serve` child comes from and how its readiness is checked.
#[derive(Debug, Clone)]
pub struct AppSpec {
    /// Path to the `spot` binary.
    pub binary: PathBuf,
    /// `--root` passed to `spot serve`.
    pub site_root: PathBuf,
    /// `host:port` of `GET /api/health/ready`.
    pub health_addr: String,
}

/// Probes a readiness endpoint. A trait so [`AppControl`] implementations and tests never
/// need a real network listener — see [`fake::FakeApp`].
#[allow(async_fn_in_trait)]
pub trait ReadyProbe: Send + Sync {
    /// `Ok(true)` when the target answers ready, `Ok(false)` when it answers but is not
    /// ready yet, `Err` on a connection failure (treated the same as "not ready yet" by
    /// [`AppControl::wait_ready`], but kept distinct for diagnostics).
    async fn probe(&self) -> Result<bool, crate::error::WinsvcError>;
}

/// A minimal raw-HTTP/1.1 `GET /api/health/ready` probe over a `TcpStream`: no HTTP
/// client dependency is in this crate's `Cargo.toml` and the brief forbids adding one the
/// brief does not name, so this reads exactly one status line.
pub struct TcpHealthProbe {
    /// `host:port` to connect to.
    pub addr: String,
    /// The request path (`/api/health/ready`).
    pub path: String,
}

impl ReadyProbe for TcpHealthProbe {
    async fn probe(&self) -> Result<bool, crate::error::WinsvcError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(&self.addr)
            .await
            .map_err(|e| crate::error::WinsvcError::io("connecting to health endpoint", &e))?;
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.path, self.addr
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| crate::error::WinsvcError::io("sending health request", &e))?;
        let mut buf = [0u8; 32];
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| crate::error::WinsvcError::io("reading health response", &e))?;
        let status_line = String::from_utf8_lossy(&buf[..n]);
        Ok(status_line.starts_with("HTTP/1.1 200") || status_line.starts_with("HTTP/1.0 200"))
    }
}

/// The real adapter: spawns `spot serve` as a child process and polls
/// [`TcpHealthProbe`] for readiness. On Windows the child is placed in a job object with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (brief: "a child with job-object kill-on-close");
/// on every platform `tokio::process::Command::kill_on_drop` is also set, so dropping the
/// handle without an explicit `stop` never leaves an orphan.
pub struct RealApp {
    spec: AppSpec,
    child: std::sync::Mutex<Option<tokio::process::Child>>,
    tx: watch::Sender<AppLifecycle>,
    // Stored as the raw handle value rather than `windows::Win32::Foundation::HANDLE`
    // itself: that type wraps a raw `*mut c_void` and so is neither `Send` nor `Sync`,
    // which this struct (behind `AppControl: Send + Sync`) requires. Nothing ever
    // dereferences the pointer directly, only passes it back into
    // `AssignProcessToJobObject`/`CloseHandle`-style Win32 calls, so round-tripping
    // through `isize` loses nothing a Win32 call needs.
    #[cfg(windows)]
    job: std::sync::Mutex<Option<isize>>,
}

impl RealApp {
    /// Builds an adapter for `spec`, not yet started.
    #[must_use]
    pub fn new(spec: AppSpec) -> Self {
        let (tx, _rx) = watch::channel(AppLifecycle::Down);
        RealApp {
            spec,
            child: std::sync::Mutex::new(None),
            tx,
            #[cfg(windows)]
            job: std::sync::Mutex::new(None),
        }
    }

    fn probe(&self) -> TcpHealthProbe {
        TcpHealthProbe {
            addr: self.spec.health_addr.clone(),
            path: "/api/health/ready".to_string(),
        }
    }
}

impl AppControl for RealApp {
    fn subscribe(&self) -> watch::Receiver<AppLifecycle> {
        self.tx.subscribe()
    }

    fn state(&self) -> AppLifecycle {
        self.tx.borrow().clone()
    }

    async fn start(&self) -> Result<(), crate::error::WinsvcError> {
        let mut command = tokio::process::Command::new(&self.spec.binary);
        command
            .arg("serve")
            .arg("--root")
            .arg(&self.spec.site_root)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null());
        #[cfg(windows)]
        {
            // `CREATE_NEW_CONSOLE` (winbase.h, 0x0000_0010): gives `spot serve` its own
            // console rather than inheriting this process's (a service has none), so
            // `crate::signal::request_graceful_stop` can later attach to that console
            // and deliver a real `CTRL_C_EVENT` — the exact signal `spot serve`'s own
            // `shutdown()` (`spot-cli/src/main.rs`) already listens for via
            // `tokio::signal::ctrl_c()`. This is harmless for a real Windows Service:
            // services run in Session 0, which has no interactive desktop, so this
            // console is never visible to a logged-on user no matter the flag.
            const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
            command.creation_flags(CREATE_NEW_CONSOLE);
        }
        let child = command
            .spawn()
            .map_err(|e| crate::error::WinsvcError::io("spawning spot serve", &e))?;
        #[cfg(windows)]
        self.assign_job_object(&child)?;
        *self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(child);
        let _ = self.tx.send(AppLifecycle::Starting);
        Ok(())
    }

    async fn wait_ready(&self, timeout: Duration) -> Result<(), crate::error::WinsvcError> {
        let probe = self.probe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if probe.probe().await.unwrap_or(false) {
                let _ = self.tx.send(AppLifecycle::Ready);
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(crate::error::WinsvcError::failed(
                    "spot serve did not become ready within the timeout",
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn stop(&self, grace: Duration) -> Result<(), crate::error::WinsvcError> {
        let child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(mut child) = child else {
            let _ = self.tx.send(AppLifecycle::Down);
            return Ok(());
        };
        crate::signal::request_graceful_stop(&child);
        let waited = tokio::time::timeout(grace, child.wait()).await;
        if waited.is_err() {
            let _ = child.kill().await;
        }
        let _ = self.tx.send(AppLifecycle::Down);
        Ok(())
    }
}

#[cfg(windows)]
impl RealApp {
    /// Creates a job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and assigns the
    /// child process to it, so the child dies if this process's job handle ever closes
    /// without an explicit stop (a crash of the service host itself).
    fn assign_job_object(
        &self,
        child: &tokio::process::Child,
    ) -> Result<(), crate::error::WinsvcError> {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        // SAFETY: `CreateJobObjectW(None, None)` creates an unnamed job object owned by
        // this process; it is a plain FFI call with no aliasing concerns.
        let job = unsafe { CreateJobObjectW(None, None) }
            .map_err(|e| crate::error::WinsvcError::failed(format!("creating job object: {e}")))?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `info` is a valid, correctly-sized structure for
        // `JobObjectExtendedLimitInformation`, as the Win32 API requires.
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info).cast(),
                u32::try_from(std::mem::size_of_val(&info)).unwrap_or(0),
            )
        }
        .map_err(|e| crate::error::WinsvcError::failed(format!("configuring job object: {e}")))?;
        let Some(raw_handle) = child.raw_handle() else {
            return Err(crate::error::WinsvcError::failed(
                "spawned child has no process handle",
            ));
        };
        // SAFETY: `raw_handle` is the freshly spawned child's own process handle.
        unsafe { AssignProcessToJobObject(job, HANDLE(raw_handle)) }
            .map_err(|e| crate::error::WinsvcError::failed(format!("assigning job object: {e}")))?;
        *self
            .job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(job.0 as isize);
        Ok(())
    }
}

#[cfg(test)]
pub mod fake {
    //! An in-memory [`AppControl`] for orchestrator tests.
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// An [`AppControl`] whose state is set by the test.
    pub struct FakeApp {
        tx: watch::Sender<AppLifecycle>,
        starts: AtomicU32,
        stops: AtomicU32,
        ready_after_polls: AtomicU32,
        fail_wait_ready: std::sync::atomic::AtomicBool,
        fail_start: std::sync::atomic::AtomicBool,
    }

    impl FakeApp {
        /// A fake starting `Down`, becoming `Ready` after `ready_after_polls` calls to
        /// [`AppControl::wait_ready`] (0 means "ready on the first poll").
        #[must_use]
        pub fn new(ready_after_polls: u32) -> Arc<Self> {
            let (tx, _rx) = watch::channel(AppLifecycle::Down);
            Arc::new(FakeApp {
                tx,
                starts: AtomicU32::new(0),
                stops: AtomicU32::new(0),
                ready_after_polls: AtomicU32::new(ready_after_polls),
                fail_wait_ready: std::sync::atomic::AtomicBool::new(false),
                fail_start: std::sync::atomic::AtomicBool::new(false),
            })
        }

        /// Makes the next [`AppControl::wait_ready`] return an error instead of ever
        /// reporting ready (simulates `spot serve` starting but never answering health).
        pub fn fail_next_wait_ready(&self) {
            self.fail_wait_ready.store(true, Ordering::SeqCst);
        }

        /// Makes the next [`AppControl::start`] return an error (simulates `spot serve`
        /// never spawning, e.g. a missing binary).
        pub fn fail_next_start(&self) {
            self.fail_start.store(true, Ordering::SeqCst);
        }

        /// Publishes a crash (an unsolicited `Exited`, as a monitor task would).
        pub fn crash(&self, code: Option<i32>) {
            let _ = self.tx.send(AppLifecycle::Exited { code });
        }

        /// How many times [`AppControl::start`] was called.
        #[must_use]
        pub fn start_count(&self) -> u32 {
            self.starts.load(Ordering::SeqCst)
        }

        /// How many times [`AppControl::stop`] was called.
        #[must_use]
        pub fn stop_count(&self) -> u32 {
            self.stops.load(Ordering::SeqCst)
        }
    }

    impl AppControl for FakeApp {
        fn subscribe(&self) -> watch::Receiver<AppLifecycle> {
            self.tx.subscribe()
        }

        fn state(&self) -> AppLifecycle {
            self.tx.borrow().clone()
        }

        async fn start(&self) -> Result<(), crate::error::WinsvcError> {
            if self.fail_start.swap(false, Ordering::SeqCst) {
                return Err(crate::error::WinsvcError::failed(
                    "fake spot serve failed to spawn",
                ));
            }
            self.starts.fetch_add(1, Ordering::SeqCst);
            let _ = self.tx.send(AppLifecycle::Starting);
            Ok(())
        }

        async fn wait_ready(&self, _timeout: Duration) -> Result<(), crate::error::WinsvcError> {
            if self.fail_wait_ready.swap(false, Ordering::SeqCst) {
                return Err(crate::error::WinsvcError::failed(
                    "fake app never became ready",
                ));
            }
            loop {
                let remaining = self.ready_after_polls.load(Ordering::SeqCst);
                if remaining == 0 {
                    let _ = self.tx.send(AppLifecycle::Ready);
                    return Ok(());
                }
                self.ready_after_polls
                    .store(remaining - 1, Ordering::SeqCst);
            }
        }

        async fn stop(&self, _grace: Duration) -> Result<(), crate::error::WinsvcError> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            let _ = self.tx.send(AppLifecycle::Down);
            Ok(())
        }
    }
}
