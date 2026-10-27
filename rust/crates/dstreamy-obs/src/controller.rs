//! The controller thread: it owns the dock model, the config and all service I/O; procs only admit work to it.

use std::ffi::CString;
use std::io;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::thread::JoinHandle;

use dstreamy_obs_producer::Endpoint;
use dstreamy_obs_producer::control::{
    ControlError, Live, Named, ObsControl, Ownership, StartCertainty, StartFailed, StartRequest,
    Started, Stopped,
};
use dstreamy_obs_producer::dock::{Canvas, DockModel, DockSettings};
use dstreamy_obs_sys as sys;
use futures_util::FutureExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::config::{ConfigSnapshot, ConfigStore};
use crate::guard;
use crate::proc::{
    Admitted, Calldata, Gate, Priority, QUEUE, Request, SHUTTING_DOWN, SIGNAL, Shared, set_str,
};

pub const START_UNWIRED: &str = "Starting D-Streamy from OBS is not available in this build yet";
pub const STOP_UNWIRED: &str = "Stopping D-Streamy from OBS is not available in this build yet";

pub trait Service {
    async fn status(&self) -> Result<Option<Live>, ControlError>;
    async fn guilds(&self) -> Result<Vec<Named>, ControlError>;
    async fn channels(&self, guild_id: &str) -> Result<Vec<Named>, ControlError>;
}

/// The service and local output a start or stop drives; nothing provides it yet.
pub trait Lifecycle {
    fn canvas(&self) -> Result<Canvas, String>;
    async fn start<F>(
        &mut self,
        request: &StartRequest,
        persisted: Option<&Ownership>,
        certainty: StartCertainty,
        checkpoint: F,
    ) -> Result<Started, StartFailed>
    where
        F: FnMut(&Ownership) -> Result<(), ControlError>;
    async fn stop(
        &mut self,
        owned: Option<&Ownership>,
        certainty: StartCertainty,
    ) -> Result<Stopped, ControlError>;
}

pub enum Unwired {}

impl Lifecycle for Unwired {
    fn canvas(&self) -> Result<Canvas, String> {
        match *self {}
    }

    async fn start<F>(
        &mut self,
        _: &StartRequest,
        _: Option<&Ownership>,
        _: StartCertainty,
        _: F,
    ) -> Result<Started, StartFailed>
    where
        F: FnMut(&Ownership) -> Result<(), ControlError>,
    {
        match *self {}
    }

    async fn stop(
        &mut self,
        _: Option<&Ownership>,
        _: StartCertainty,
    ) -> Result<Stopped, ControlError> {
        match *self {}
    }
}

pub struct Parts<S, L> {
    pub store: ConfigStore,
    pub service: S,
    pub lifecycle: Option<L>,
    pub signals: *mut sys::signal_handler_t,
}

pub struct ControllerHandle {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl ControllerHandle {
    /// Cancels every wait and stops the controller, without blocking.
    pub fn cancel_unload(&self) {
        self.shared.priority.cancel_unload();
    }

    pub fn join(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn spawn<S, L>(gate: &'static Gate, parts: Parts<S, L>) -> io::Result<ControllerHandle>
where
    S: Service + Send + 'static,
    L: Lifecycle + Send + 'static,
{
    let Parts {
        store,
        service,
        lifecycle,
        signals,
    } = parts;
    let emitter = Emitter(signals);
    let (tx, requests) = mpsc::channel(QUEUE);
    let shared = Arc::new(Shared::default());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let inner = shared.clone();
    let thread = std::thread::Builder::new()
        .name("dstreamy control".into())
        .spawn(move || {
            let mut controller =
                Controller::new(store, service, lifecycle, emitter, inner, requests);
            let run = AssertUnwindSafe(controller.run()).catch_unwind();
            if let Err(payload) = runtime.block_on(run) {
                guard::record_panic("controller", payload.as_ref());
                gate.close();
            }
            controller.finish();
        })?;
    gate.open(tx, shared.clone());
    Ok(ControllerHandle {
        shared,
        thread: Some(thread),
    })
}

/// The service at the user's default endpoint.
pub struct Remote(Result<ObsControl, String>);

impl Remote {
    pub fn for_user() -> Self {
        Self(
            Endpoint::default_for_user()
                .map(ObsControl::new)
                .map_err(|e| e.to_string()),
        )
    }

    fn control(&self) -> Result<&ObsControl, ControlError> {
        self.0
            .as_ref()
            .map_err(|e| ControlError::Connect(e.clone()))
    }
}

impl Service for Remote {
    async fn status(&self) -> Result<Option<Live>, ControlError> {
        self.control()?.status().await
    }

    async fn guilds(&self) -> Result<Vec<Named>, ControlError> {
        self.control()?.guilds().await
    }

    async fn channels(&self, guild_id: &str) -> Result<Vec<Named>, ControlError> {
        self.control()?.channels(guild_id).await
    }
}

struct Emitter(*mut sys::signal_handler_t);

// SAFETY: libobs signal handlers may be signalled from any thread.
unsafe impl Send for Emitter {}

type Outcome = Result<Option<Value>, String>;

/// The ownership and certainty last saved; the model's `owned` only mirrors it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Journal {
    owned: Option<Ownership>,
    certainty: StartCertainty,
}

impl Journal {
    fn snapshot(&self, settings: &DockSettings) -> ConfigSnapshot {
        ConfigSnapshot {
            settings: DockSettings {
                owned: self.owned.clone(),
                ..settings.clone()
            },
            ownership_certainty: self.certainty,
        }
    }
}

struct Controller<S, L> {
    model: DockModel,
    journal: Journal,
    store: ConfigStore,
    service: S,
    lifecycle: Option<L>,
    emitter: Emitter,
    shared: Arc<Shared>,
    requests: mpsc::Receiver<Admitted>,
}

impl<S: Service, L: Lifecycle> Controller<S, L> {
    fn new(
        store: ConfigStore,
        service: S,
        lifecycle: Option<L>,
        emitter: Emitter,
        shared: Arc<Shared>,
        requests: mpsc::Receiver<Admitted>,
    ) -> Self {
        Self {
            model: DockModel::new(DockSettings::default()),
            journal: Journal::default(),
            store,
            service,
            lifecycle,
            emitter,
            shared,
            requests,
        }
    }

    fn load(&mut self) -> Outcome {
        let snapshot = self.store.load()?;
        self.journal = Journal {
            owned: snapshot.settings.owned.clone(),
            certainty: snapshot.ownership_certainty,
        };
        self.model = DockModel::new(snapshot.settings);
        Ok(None)
    }

    #[cfg(test)]
    fn journal(&self) -> (Option<Ownership>, StartCertainty) {
        (self.journal.owned.clone(), self.journal.certainty)
    }

    async fn run(&mut self) {
        let loaded = self.load();
        self.emit(0, "ready", loaded);
        let shared = self.shared.clone();
        loop {
            if shared.priority.unloading() {
                return;
            }
            if let Some(id) = shared.priority.take_shutdown() {
                self.shutdown(id);
                shared.priority.until(Priority::unloading).await;
                return;
            }
            tokio::select! {
                biased;
                () = shared.priority.until(Priority::shutting_down) => {}
                () = shared.mailbox.changed() => {
                    if let Some(operation) = shared.mailbox.take_change() {
                        self.emit(0, operation, Ok(None));
                    }
                }
                Some(Admitted { id, request }) = self.requests.recv() => {
                    let operation = request.operation();
                    let outcome = self.handle(request).await;
                    shared.finish(id);
                    if !shared.priority.unloading() {
                        self.emit(id, operation, outcome);
                    }
                }
            }
        }
    }

    async fn handle(&mut self, request: Request) -> Outcome {
        match request {
            Request::GetSettings => Ok(None),
            Request::SetSettings { json } => self.set_settings(&json).map(|()| None),
            Request::Status => self.status().await,
            Request::Guilds => self.guilds().await,
            Request::Channels { guild_id } => self.channels(&guild_id).await,
            Request::Start => self.start().await,
            Request::Stop => self.stop().await,
            Request::Shutdown => Err(SHUTTING_DOWN.into()),
        }
    }

    fn set_settings(&mut self, json: &str) -> Result<(), String> {
        let settings = DockSettings::from_external_json(json)?;
        self.store.check_writable()?;
        let mut next = self.model.clone();
        next.set_settings(settings);
        self.store.save(&self.journal.snapshot(next.settings()))?;
        self.model = next;
        Ok(())
    }

    async fn status(&mut self) -> Outcome {
        let live = cancelable(&self.shared, self.service.status())
            .await?
            .map_err(|e| e.to_string())?;
        let session = live.map(|l| {
            json!({
                "guild_id": l.guild_id, "channel_id": l.channel_id, "channel_name": l.channel_name,
                "target": l.target, "started_by": l.started_by,
            })
        });
        Ok(Some(json!({ "session": session })))
    }

    async fn guilds(&mut self) -> Outcome {
        let result = cancelable(&self.shared, self.service.guilds()).await?;
        let error = result.as_ref().err().map(ToString::to_string);
        self.model.guilds_loaded(result);
        error.map_or(Ok(None), Err)
    }

    async fn channels(&mut self, guild_id: &str) -> Outcome {
        let result = cancelable(&self.shared, self.service.channels(guild_id)).await?;
        let error = result.as_ref().err().map(ToString::to_string);
        self.model.channels_listed(guild_id, result);
        error.map_or(Ok(None), Err)
    }

    async fn start(&mut self) -> Outcome {
        self.store.check_writable()?;
        let Some(lifecycle) = self.lifecycle.as_mut() else {
            return Err(START_UNWIRED.into());
        };
        let request = self.model.start(lifecycle.canvas()?)?;
        let prior = self.journal.clone();
        let settings = self.model.settings().clone();
        let store = &self.store;
        let journal = &mut self.journal;
        let mut checkpointed = false;
        let checkpoint = |ownership: &Ownership| {
            let pending = Journal {
                owned: Some(ownership.clone()),
                certainty: StartCertainty::Pending,
            };
            store
                .save(&pending.snapshot(&settings))
                .map_err(ControlError::Persistence)?;
            *journal = pending;
            checkpointed = true;
            Ok(())
        };
        let started = lifecycle.start(&request, prior.owned.as_ref(), prior.certainty, checkpoint);
        let (definitive, error) = match cancelable(&self.shared, started).await {
            Ok(Ok(started)) => {
                let confirmed = Journal {
                    owned: started.ownership,
                    certainty: StartCertainty::Confirmed,
                };
                let saved = self.store.save(&confirmed.snapshot(&settings));
                if saved.is_ok() {
                    self.journal = confirmed;
                }
                self.model.started(Ok(Started {
                    ownership: self.journal.owned.clone(),
                    channel: started.channel,
                }));
                return saved.map(|()| None).map_err(|e| {
                    format!("D-Streamy started, but its session record was not saved: {e}")
                });
            }
            Ok(Err(failed)) => (failed.ownership.is_none(), failed.error),
            Err(cancelled) => (false, ControlError::Connect(cancelled)),
        };
        if checkpointed && definitive && self.store.save(&prior.snapshot(&settings)).is_ok() {
            self.journal = prior;
        }
        let message = error.to_string();
        self.model.started(Err(StartFailed {
            error,
            ownership: self.journal.owned.clone().map(Box::new),
        }));
        Err(message)
    }

    async fn stop(&mut self) -> Outcome {
        self.shared.priority.take_stop();
        let Some(lifecycle) = self.lifecycle.as_mut() else {
            return Err(STOP_UNWIRED.into());
        };
        if self.model.stop().is_none() {
            return Err("D-Streamy is not running".into());
        }
        let stopped = lifecycle.stop(self.journal.owned.as_ref(), self.journal.certainty);
        let result = match cancelable(&self.shared, stopped).await {
            Ok(Ok(_)) => {
                let cleared = Journal {
                    owned: None,
                    certainty: StartCertainty::Confirmed,
                };
                let saved = self.store.save(&cleared.snapshot(self.model.settings()));
                if saved.is_ok() {
                    self.journal = cleared;
                }
                saved
            }
            Ok(Err(e)) => Err(e.to_string()),
            Err(e) => Err(e),
        };
        self.model.stopped(result.clone());
        result.map(|()| None)
    }

    fn shutdown(&mut self, id: i64) {
        self.shared.mailbox.close();
        self.requests.close();
        while let Ok(Admitted { id, request }) = self.requests.try_recv() {
            self.shared.finish(id);
            self.emit(id, request.operation(), Err(SHUTTING_DOWN.into()));
        }
        self.shared.finish(id);
        self.emit(id, "shutdown", Ok(None));
    }

    fn finish(&mut self) {
        self.shared.mailbox.close();
        self.requests.close();
        while self.requests.try_recv().is_ok() {}
        self.shared.clear();
    }

    fn emit(&mut self, id: i64, operation: &str, outcome: Outcome) {
        let (ok, service) = match outcome {
            Ok(service) => (true, service),
            Err(e) => {
                self.model.report_error(e);
                (false, None)
            }
        };
        if self.model.error().is_none()
            && let Some(recovery) = self.store.recovery()
        {
            self.model.report_error(recovery.to_owned());
        }
        let mut v = self.model.view();
        let seen = self.shared.mailbox.view();
        v["request_id"] = id.into();
        v["operation"] = operation.into();
        v["ok"] = ok.into();
        v["service"] = service.unwrap_or(Value::Null);
        v["output"] = json!({
            "active": false,
            "video": null,
            "stream_active": seen.is_some_and(|s| !s.stopped),
            "started_by_stream": false,
            "generation": seen.map(|s| s.generation),
        });
        let Ok(text) = CString::new(v.to_string()) else {
            return;
        };
        let mut cd = Calldata::new();
        // SAFETY: the handler outlives the controller thread, which unload joins first; no lock is held here.
        unsafe {
            set_str(cd.as_ptr(), c"json", &text);
            sys::signal_handler_signal(self.emitter.0, SIGNAL.as_ptr(), cd.as_ptr());
        }
    }
}

/// Runs `work` until unload; a graceful shutdown lets it finish.
async fn cancelable<T>(shared: &Shared, work: impl Future<Output = T>) -> Result<T, String> {
    tokio::select! {
        biased;
        () = shared.priority.until(Priority::unloading) => Err(SHUTTING_DOWN.into()),
        value = work => Ok(value),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::time::{Duration, Instant};

    use dstreamy_obs_producer::control::{Candidate, Instance};
    use tokio::sync::Semaphore;

    use super::*;
    use crate::proc::get_str;
    use crate::proc::tests::{
        Arg, FAKE_REFS, Handlers, Reply, accepted, call, fake_output, held, refused,
    };
    use crate::testdir;

    const WAIT: Duration = Duration::from_secs(5);
    const INSTANCE: Instance = Instance {
        dev: 1,
        ino: 2,
        mtime: 3,
        mtime_nsec: 4,
    };
    const CANVAS: Canvas = Canvas {
        fps_num: 30,
        fps_den: 1,
        output_width: 1920,
        output_height: 1080,
    };

    fn owner(id: u64) -> Ownership {
        Ownership {
            connections: vec![Candidate { id, fps: Some(30) }],
            guild_id: "g".into(),
            channel_id: "c".into(),
            instance: Some(INSTANCE),
        }
    }

    type Reenter = Box<dyn FnMut(&Value) + Send>;

    #[derive(Default)]
    struct Collector {
        seen: Mutex<Vec<Value>>,
        cond: Condvar,
        reenter: Mutex<Option<Reenter>>,
    }

    unsafe extern "C" fn on_state(data: *mut c_void, cd: *mut sys::calldata_t) {
        // SAFETY: `data` is the rig's boxed collector, which outlives the signal handler.
        let collector = unsafe { &*(data as *const Collector) };
        // SAFETY: the signal's calldata.
        let json = unsafe { get_str(cd, c"json") }.expect("json string");
        let value: Value = serde_json::from_slice(json.to_bytes()).expect("valid JSON");
        if let Some(reenter) = collector.reenter.lock().unwrap().as_mut() {
            reenter(&value);
        }
        collector.seen.lock().unwrap().push(value);
        collector.cond.notify_all();
    }

    impl Collector {
        fn find(&self, id: i64, operation: &str) -> Option<Value> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .find(|v| v["request_id"] == id && v["operation"] == operation)
                .cloned()
        }

        fn wait(&self, id: i64, operation: &str) -> Value {
            let deadline = Instant::now() + WAIT;
            let mut seen = self.seen.lock().unwrap();
            loop {
                if let Some(v) = seen
                    .iter()
                    .find(|v| v["request_id"] == id && v["operation"] == operation)
                {
                    return v.clone();
                }
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !left.is_zero(),
                    "no {operation} completion for {id}; saw {seen:?}"
                );
                seen = self.cond.wait_timeout(seen, left).unwrap().0;
            }
        }

        fn count(&self, id: i64) -> usize {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|v| v["request_id"] == id)
                .count()
        }
    }

    #[derive(Clone, Default)]
    struct FakeService {
        live: Option<Live>,
        fail: Option<ControlError>,
        block: bool,
        hold: Option<Arc<Semaphore>>,
        calls: Arc<AtomicUsize>,
    }

    impl Service for FakeService {
        async fn status(&self) -> Result<Option<Live>, ControlError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.block {
                std::future::pending::<()>().await;
            }
            if let Some(hold) = &self.hold {
                hold.acquire().await.unwrap().forget();
            }
            match &self.fail {
                Some(e) => Err(e.clone()),
                None => Ok(self.live.clone()),
            }
        }

        async fn guilds(&self) -> Result<Vec<Named>, ControlError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![
                Named {
                    id: "g".into(),
                    name: "Guild".into(),
                },
                Named {
                    id: "h".into(),
                    name: "Other".into(),
                },
            ])
        }

        async fn channels(&self, guild_id: &str) -> Result<Vec<Named>, ControlError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![Named {
                id: format!("{guild_id}1"),
                name: format!("voice of {guild_id}"),
            }])
        }
    }

    #[derive(Clone, Copy)]
    enum Script {
        Succeed,
        Lose,
        Refuse,
        Hang,
    }

    type Stops = Arc<Mutex<Vec<(Option<Ownership>, StartCertainty)>>>;

    struct FakeLifecycle {
        log: Arc<Mutex<Vec<&'static str>>>,
        start: Script,
        stop: Result<Stopped, ControlError>,
        pause: Option<(std::sync::mpsc::Sender<()>, Arc<Semaphore>)>,
        stops: Stops,
    }

    impl FakeLifecycle {
        fn new(
            start: Script,
            stop: Result<Stopped, ControlError>,
        ) -> (Self, Arc<Mutex<Vec<&'static str>>>) {
            let log = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    log: log.clone(),
                    start,
                    stop,
                    pause: None,
                    stops: Stops::default(),
                },
                log,
            )
        }
    }

    impl Lifecycle for FakeLifecycle {
        fn canvas(&self) -> Result<Canvas, String> {
            Ok(CANVAS)
        }

        async fn start<F>(
            &mut self,
            request: &StartRequest,
            _: Option<&Ownership>,
            _: StartCertainty,
            mut checkpoint: F,
        ) -> Result<Started, StartFailed>
        where
            F: FnMut(&Ownership) -> Result<(), ControlError>,
        {
            self.log.lock().unwrap().push("start");
            let provisional = Ownership {
                connections: vec![Candidate {
                    id: 9,
                    fps: Some(request.fps),
                }],
                guild_id: request.guild_id.clone(),
                channel_id: request.channel_id.clone(),
                instance: Some(INSTANCE),
            };
            if let Err(error) = checkpoint(&provisional) {
                return Err(StartFailed {
                    error,
                    ownership: None,
                });
            }
            self.log.lock().unwrap().push("sent");
            if let Some((checkpointed, resume)) = &self.pause {
                checkpointed.send(()).unwrap();
                resume.acquire().await.unwrap().forget();
            }
            match self.start {
                Script::Hang => std::future::pending().await,
                Script::Succeed => Ok(Started {
                    ownership: Some(provisional.confirmed(9)),
                    channel: request.channel_id.clone(),
                }),
                Script::Lose => Err(StartFailed {
                    error: ControlError::Timeout {
                        command: "start",
                        secs: 1,
                    },
                    ownership: Some(Box::new(provisional)),
                }),
                Script::Refuse => Err(StartFailed {
                    error: ControlError::Refused {
                        code: "unknown_channel".into(),
                        message: "no such channel".into(),
                    },
                    ownership: None,
                }),
            }
        }

        async fn stop(
            &mut self,
            owned: Option<&Ownership>,
            certainty: StartCertainty,
        ) -> Result<Stopped, ControlError> {
            self.log.lock().unwrap().push("stop");
            self.stops.lock().unwrap().push((owned.cloned(), certainty));
            self.stop.clone()
        }
    }

    struct Rig {
        gate: &'static Gate,
        handle: Option<ControllerHandle>,
        handlers: Handlers,
        collector: Box<Collector>,
        path: PathBuf,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.unload();
        }
    }

    impl Rig {
        fn new<L: Lifecycle + Send + 'static>(
            path: PathBuf,
            service: FakeService,
            lifecycle: Option<L>,
        ) -> Self {
            let gate: &'static Gate = Box::leak(Box::new(Gate::new(FAKE_REFS)));
            let handlers = Handlers::new();
            assert!(gate.register(handlers.procs, handlers.signals));
            let collector = Box::new(Collector::default());
            // SAFETY: the collector outlives the handler, which the rig destroys after joining.
            unsafe {
                sys::signal_handler_connect(
                    handlers.signals,
                    SIGNAL.as_ptr(),
                    Some(on_state),
                    (&raw const *collector).cast_mut().cast(),
                )
            };
            let handle = spawn(
                gate,
                Parts {
                    store: ConfigStore::at(path.clone()),
                    service,
                    lifecycle,
                    signals: handlers.signals,
                },
            )
            .unwrap();
            let rig = Self {
                gate,
                handle: Some(handle),
                handlers,
                collector,
                path,
            };
            rig.collector.wait(0, "ready");
            rig
        }

        fn plain(path: PathBuf) -> Self {
            Self::new::<Unwired>(path, FakeService::default(), None)
        }

        fn call(&self, name: &str, args: &[Arg]) -> Reply {
            call(self.handlers.procs, name, args)
        }

        fn request(&self, name: &str, id: i64) -> Value {
            assert_eq!(
                self.call(name, &[Arg::Int("request_id", id)]),
                accepted(),
                "{name}"
            );
            self.collector
                .wait(id, name.trim_start_matches("dstreamy_"))
        }

        fn set_settings(&self, id: i64, json: &str) -> Value {
            assert_eq!(
                self.call(
                    "dstreamy_set_settings",
                    &[
                        Arg::Int("request_id", id),
                        Arg::Str("json", json.as_bytes())
                    ]
                ),
                accepted()
            );
            self.collector.wait(id, "set_settings")
        }

        fn channels(&self, id: i64, guild: &str) -> Value {
            assert_eq!(
                self.call(
                    "dstreamy_channels",
                    &[
                        Arg::Int("request_id", id),
                        Arg::Str("guild_id", guild.as_bytes())
                    ]
                ),
                accepted()
            );
            self.collector.wait(id, "channels")
        }

        fn position(&self, id: i64, operation: &str) -> usize {
            self.collector
                .seen
                .lock()
                .unwrap()
                .iter()
                .position(|v| v["request_id"] == id && v["operation"] == operation)
                .unwrap()
        }

        fn unload(&mut self) {
            self.gate.close();
            if let Some(handle) = self.handle.take() {
                handle.cancel_unload();
                handle.join();
            }
        }

        fn disk(&self) -> Result<ConfigSnapshot, String> {
            ConfigStore::at(self.path.clone()).load()
        }
    }

    fn v1_with(owned: &Ownership, certainty: &str) -> String {
        json!({
            "version": 1, "guild_id": "g", "channel_id": "c",
            "owned": serde_json::to_string(owned).unwrap(), "ownership_certainty": certainty,
        })
        .to_string()
    }

    fn bytes(p: &Path) -> Vec<u8> {
        fs::read(p).unwrap()
    }

    fn backup(p: &Path) -> PathBuf {
        p.with_extension("json.bak")
    }

    fn assert_fields(v: &Value) {
        let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "channels",
                "error",
                "guilds",
                "ok",
                "operation",
                "output",
                "request_id",
                "service",
                "settings",
                "state",
            ]
        );
    }

    #[test]
    fn ready_and_get_settings_carry_the_full_snapshot() {
        let dir = testdir::fresh("ctl-snapshot");
        let path = dir.join("config.json");
        fs::write(
            &path,
            json!({"version": 1, "guild_id": "g", "mode": "share"}).to_string(),
        )
        .unwrap();
        let rig = Rig::plain(path);
        let ready = rig.collector.wait(0, "ready");
        assert_fields(&ready);
        assert_eq!(ready["ok"], true);
        assert_eq!(ready["state"], json!({"state": "idle"}));
        assert_eq!(ready["settings"]["guild_id"], "g");
        assert_eq!(ready["settings"]["mode"], "share");
        assert_eq!(ready["service"], Value::Null);
        assert_eq!(
            ready["output"],
            json!({"active": false, "video": null, "stream_active": false, "started_by_stream": false, "generation": null})
        );
        let got = rig.request("dstreamy_get_settings", 5);
        assert_fields(&got);
        assert_eq!(got["ok"], true);
        for key in ["state", "error", "guilds", "channels", "settings", "output"] {
            assert_eq!(got[key], ready[key], "{key}");
        }
    }

    #[test]
    fn accepted_is_not_completed() {
        let dir = testdir::fresh("ctl-accepted");
        let service = FakeService {
            block: true,
            ..Default::default()
        };
        let rig = Rig::new::<Unwired>(dir.join("config.json"), service.clone(), None);
        assert_eq!(
            rig.call("dstreamy_status", &[Arg::Int("request_id", 3)]),
            accepted()
        );
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(rig.collector.count(3), 0);
        assert_eq!(service.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn settings_are_saved_before_success_and_strictly_checked() {
        let dir = testdir::fresh("ctl-settings");
        let path = dir.join("config.json");
        let rig = Rig::plain(path.clone());
        let done = rig.set_settings(
            1,
            r#"{"guild_id": "g", "bitrate_kbps": 9000, "mode": "share"}"#,
        );
        assert_eq!(done["ok"], true, "{done}");
        assert_eq!(done["settings"]["bitrate_kbps"], 9000);
        let disk = rig.disk().unwrap();
        assert_eq!(disk.settings.bitrate_kbps, 9000);
        assert_eq!(disk.settings.guild_id.as_deref(), Some("g"));
        let before = bytes(&path);

        for (id, bad) in [
            (2, r#"{"bitrate_kbps": "x"}"#),
            (3, "[]"),
            (4, r#"{"bitrate_kbps": 10}"#),
            (5, "{"),
        ] {
            let done = rig.set_settings(id, bad);
            assert_eq!(done["ok"], false, "{bad}");
            assert!(done["error"].is_string(), "{bad}");
            assert_eq!(done["settings"]["bitrate_kbps"], 9000);
        }
        assert_eq!(bytes(&path), before);
    }

    #[test]
    fn settings_cannot_set_or_clear_the_journal() {
        let dir = testdir::fresh("ctl-journal");
        let path = dir.join("config.json");
        let a = owner(3);
        fs::write(&path, v1_with(&a, "pending")).unwrap();
        let rig = Rig::plain(path);
        let done = rig.set_settings(
            1,
            r#"{"guild_id": "h", "owned": null, "ownership_certainty": "confirmed"}"#,
        );
        assert_eq!(done["ok"], true, "{done}");
        assert_eq!(done["settings"]["owned"], serde_json::to_value(&a).unwrap());
        let disk = rig.disk().unwrap();
        assert_eq!(disk.settings.owned, Some(a));
        assert_eq!(disk.ownership_certainty, StartCertainty::Pending);
        assert_eq!(disk.settings.guild_id.as_deref(), Some("h"));
    }

    #[test]
    fn without_a_lifecycle_start_and_stop_fail_explicitly() {
        let dir = testdir::fresh("ctl-unwired");
        let path = dir.join("config.json");
        fs::write(&path, v1_with(&owner(3), "confirmed")).unwrap();
        let before = bytes(&path);
        let rig = Rig::plain(path.clone());
        let start = rig.request("dstreamy_start", 1);
        assert_eq!(
            (start["ok"].clone(), start["error"].clone()),
            (json!(false), json!(START_UNWIRED))
        );
        assert_eq!(start["state"], json!({"state": "idle"}));
        assert_eq!(start["output"]["active"], false);
        let stop = rig.request("dstreamy_stop", 2);
        assert_eq!(
            (stop["ok"].clone(), stop["error"].clone()),
            (json!(false), json!(STOP_UNWIRED))
        );
        assert_eq!(bytes(&path), before);
    }

    #[test]
    fn a_writable_config_lets_a_start_through() {
        let dir = testdir::fresh("ctl-start-ok");
        let path = dir.join("config.json");
        fs::write(
            &path,
            json!({"version": 1, "guild_id": "g", "channel_id": "c"}).to_string(),
        )
        .unwrap();
        let (lifecycle, log) = FakeLifecycle::new(Script::Succeed, Ok(Stopped::Stopped));
        let rig = Rig::new(path, FakeService::default(), Some(lifecycle));
        let done = rig.request("dstreamy_start", 1);
        assert_eq!(done["ok"], true, "{done}");
        assert_eq!(*log.lock().unwrap(), ["start", "sent"]);
        let disk = rig.disk().unwrap();
        assert_eq!(disk.ownership_certainty, StartCertainty::Confirmed);
        assert_eq!(disk.settings.owned, Some(owner(9).confirmed(9)));
        assert_eq!(done["output"]["active"], false);
    }

    #[test]
    fn read_only_recovery_sends_no_start_and_clears_no_owner() {
        let dir = testdir::fresh("ctl-recovery");
        let path = dir.join("config.json");
        let a = owner(3);
        fs::write(
            &path,
            b"{\"version\": 1, \"owned\": \"{\\\"connections\\\":[{\\\"id\\\":7",
        )
        .unwrap();
        fs::write(backup(&path), v1_with(&a, "pending")).unwrap();
        let files = (bytes(&path), bytes(&backup(&path)));
        let (lifecycle, log) = FakeLifecycle::new(Script::Succeed, Ok(Stopped::NotRunning));
        let rig = Rig::new(path.clone(), FakeService::default(), Some(lifecycle));

        let ready = rig.collector.wait(0, "ready");
        assert_eq!(
            ready["settings"]["owned"],
            serde_json::to_value(&a).unwrap()
        );
        assert!(
            ready["error"].as_str().unwrap().contains("config.json"),
            "{ready}"
        );

        let start = rig.request("dstreamy_start", 1);
        assert_eq!(start["ok"], false);
        assert!(log.lock().unwrap().is_empty(), "no start may be dispatched");

        let stop = rig.request("dstreamy_stop", 2);
        assert_eq!(stop["ok"], false, "{stop}");
        assert_eq!(*log.lock().unwrap(), ["stop"]);
        assert_eq!(stop["settings"]["owned"], serde_json::to_value(&a).unwrap());

        let again = rig.request("dstreamy_start", 3);
        assert_eq!(again["ok"], false);
        assert_eq!(*log.lock().unwrap(), ["stop"]);
        let set = rig.set_settings(4, r#"{"bitrate_kbps": 7000}"#);
        assert_eq!(set["ok"], false);
        let status = rig.request("dstreamy_status", 5);
        assert_eq!(status["ok"], true);
        assert_eq!((bytes(&path), bytes(&backup(&path))), files);
    }

    #[test]
    fn a_proven_replacement_clears_the_owner_only_when_writable() {
        let dir = testdir::fresh("ctl-clear");
        let path = dir.join("config.json");
        fs::write(&path, v1_with(&owner(3), "pending")).unwrap();
        let (lifecycle, log) = FakeLifecycle::new(Script::Succeed, Ok(Stopped::NotRunning));
        let rig = Rig::new(path, FakeService::default(), Some(lifecycle));
        let stop = rig.request("dstreamy_stop", 1);
        assert_eq!(stop["ok"], true, "{stop}");
        assert_eq!(*log.lock().unwrap(), ["stop"]);
        assert_eq!(stop["settings"]["owned"], Value::Null);
        let disk = rig.disk().unwrap();
        assert_eq!(
            (disk.settings.owned, disk.ownership_certainty),
            (None, StartCertainty::Confirmed)
        );
    }

    #[test]
    fn an_unresolved_stop_keeps_the_owner() {
        let dir = testdir::fresh("ctl-unresolved-stop");
        let path = dir.join("config.json");
        fs::write(&path, v1_with(&owner(3), "pending")).unwrap();
        let before = bytes(&path);
        let (lifecycle, _) =
            FakeLifecycle::new(Script::Succeed, Err(ControlError::UnresolvedStart));
        let rig = Rig::new(path.clone(), FakeService::default(), Some(lifecycle));
        let stop = rig.request("dstreamy_stop", 1);
        assert_eq!(stop["ok"], false);
        assert_eq!(
            stop["settings"]["owned"],
            serde_json::to_value(owner(3)).unwrap()
        );
        assert_eq!(bytes(&path), before);
    }

    #[test]
    fn a_failed_checkpoint_sends_nothing() {
        let dir = testdir::fresh("ctl-checkpoint");
        let sub = dir.join("cfg");
        fs::create_dir_all(&sub).unwrap();
        let path = sub.join("config.json");
        fs::write(
            &path,
            json!({"version": 1, "guild_id": "g", "channel_id": "c"}).to_string(),
        )
        .unwrap();
        let before = bytes(&path);
        let (lifecycle, log) = FakeLifecycle::new(Script::Succeed, Ok(Stopped::Stopped));
        let rig = Rig::new(path.clone(), FakeService::default(), Some(lifecycle));
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o555)).unwrap();
        let done = rig.request("dstreamy_start", 1);
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(done["ok"], false);
        assert_eq!(*log.lock().unwrap(), ["start"]);
        assert_eq!(done["settings"]["owned"], Value::Null);
        assert_eq!(done["state"], json!({"state": "idle"}));
        assert_eq!(bytes(&path), before);
    }

    #[test]
    fn a_definitive_refusal_restores_the_prior_journal() {
        let dir = testdir::fresh("ctl-refused");
        let path = dir.join("config.json");
        let prior = owner(3);
        fs::write(&path, v1_with(&prior, "confirmed")).unwrap();
        let (lifecycle, log) = FakeLifecycle::new(Script::Refuse, Ok(Stopped::Stopped));
        let rig = Rig::new(path, FakeService::default(), Some(lifecycle));
        let done = rig.request("dstreamy_start", 1);
        assert_eq!(done["ok"], false);
        assert_eq!(*log.lock().unwrap(), ["start", "sent"]);
        assert_eq!(
            done["settings"]["owned"],
            serde_json::to_value(&prior).unwrap()
        );
        let disk = rig.disk().unwrap();
        assert_eq!(
            (disk.settings.owned, disk.ownership_certainty),
            (Some(prior), StartCertainty::Confirmed)
        );
    }

    #[test]
    fn the_journal_holds_the_checkpoint_when_the_start_is_dropped_after_it() {
        let dir = testdir::fresh("ctl-journal-drop");
        let path = dir.join("config.json");
        fs::write(&path, v1_with(&owner(3), "confirmed")).unwrap();
        let handlers = Handlers::new();
        let (lifecycle, log) = FakeLifecycle::new(Script::Hang, Ok(Stopped::Stopped));
        let (_tx, requests) = mpsc::channel(QUEUE);
        let mut controller = Controller::new(
            ConfigStore::at(path.clone()),
            FakeService::default(),
            Some(lifecycle),
            Emitter(handlers.signals),
            Arc::new(Shared::default()),
            requests,
        );
        controller.load().unwrap();
        assert!(controller.start().now_or_never().is_none());
        assert_eq!(*log.lock().unwrap(), ["start", "sent"]);
        assert_eq!(
            controller.journal(),
            (Some(owner(9)), StartCertainty::Pending)
        );
        let disk = ConfigStore::at(path).load().unwrap();
        assert_eq!(
            (disk.settings.owned, disk.ownership_certainty),
            (Some(owner(9)), StartCertainty::Pending)
        );
    }

    #[test]
    fn a_paused_start_is_journalled_and_a_failed_confirmation_stays_pending() {
        let dir = testdir::fresh("ctl-journal-pause");
        let sub = dir.join("cfg");
        fs::create_dir_all(&sub).unwrap();
        let path = sub.join("config.json");
        fs::write(&path, v1_with(&owner(3), "confirmed")).unwrap();
        let (mut lifecycle, log) = FakeLifecycle::new(Script::Succeed, Ok(Stopped::LeftRunning));
        let (checkpointed, paused) = std::sync::mpsc::channel();
        let resume = Arc::new(Semaphore::new(0));
        lifecycle.pause = Some((checkpointed, resume.clone()));
        let stops = lifecycle.stops.clone();
        let rig = Rig::new(path.clone(), FakeService::default(), Some(lifecycle));
        assert_eq!(
            rig.call("dstreamy_start", &[Arg::Int("request_id", 1)]),
            accepted()
        );
        paused.recv_timeout(WAIT).unwrap();
        let disk = rig.disk().unwrap();
        assert_eq!(
            (disk.settings.owned, disk.ownership_certainty),
            (Some(owner(9)), StartCertainty::Pending)
        );
        let saved = (bytes(&path), bytes(&backup(&path)));
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o555)).unwrap();
        resume.add_permits(1);
        let done = rig.collector.wait(1, "start");
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(done["ok"], false);
        assert!(
            done["error"].as_str().unwrap().contains("not saved"),
            "{done}"
        );
        assert_eq!(done["state"]["state"], "live");
        assert_eq!(
            done["settings"]["owned"],
            serde_json::to_value(owner(9)).unwrap()
        );
        assert_eq!((bytes(&path), bytes(&backup(&path))), saved);

        rig.request("dstreamy_stop", 2);
        assert_eq!(*log.lock().unwrap(), ["start", "sent", "stop"]);
        assert_eq!(
            *stops.lock().unwrap(),
            [(Some(owner(9)), StartCertainty::Pending)]
        );
    }

    #[test]
    fn a_lost_reply_keeps_the_pending_start() {
        let dir = testdir::fresh("ctl-lost");
        let path = dir.join("config.json");
        fs::write(&path, v1_with(&owner(3), "confirmed")).unwrap();
        let (lifecycle, _) = FakeLifecycle::new(Script::Lose, Ok(Stopped::Stopped));
        let rig = Rig::new(path, FakeService::default(), Some(lifecycle));
        let done = rig.request("dstreamy_start", 1);
        assert_eq!(done["ok"], false);
        assert_eq!(
            done["settings"]["owned"],
            serde_json::to_value(owner(9)).unwrap()
        );
        let disk = rig.disk().unwrap();
        assert_eq!(
            (disk.settings.owned, disk.ownership_certainty),
            (Some(owner(9)), StartCertainty::Pending)
        );
    }

    #[test]
    fn status_and_lists_use_the_service() {
        let dir = testdir::fresh("ctl-service");
        let live = Live {
            guild_id: "g".into(),
            channel_id: "c".into(),
            channel_name: Some("voice".into()),
            target: "external".into(),
            started_by: Some(4),
            instance: None,
        };
        let service = FakeService {
            live: Some(live),
            ..Default::default()
        };
        let rig = Rig::new::<Unwired>(dir.join("config.json"), service, None);
        let status = rig.request("dstreamy_status", 1);
        assert_eq!(status["ok"], true);
        assert_eq!(
            status["service"],
            json!({"session": {"guild_id": "g", "channel_id": "c", "channel_name": "voice", "target": "external", "started_by": 4}})
        );
        let guilds = rig.request("dstreamy_guilds", 2);
        assert_eq!(guilds["service"], Value::Null);
        assert_eq!(
            guilds["guilds"],
            json!([{"id": "g", "name": "Guild"}, {"id": "h", "name": "Other"}])
        );

        assert_eq!(rig.set_settings(4, r#"{"guild_id": "g"}"#)["ok"], true);
        let channels = rig.channels(5, "g");
        assert_eq!(channels["ok"], true, "{channels}");
        assert_eq!(
            channels["channels"],
            json!([{"id": "g1", "name": "voice of g"}])
        );
    }

    fn model_fields(v: &Value) -> Value {
        json!({
            "state": v["state"], "error": v["error"], "guilds": v["guilds"],
            "channels": v["channels"], "settings": v["settings"],
        })
    }

    #[test]
    fn channels_of_an_unsaved_guild_are_listed_without_saving_anything() {
        let dir = testdir::fresh("ctl-browse");
        let path = dir.join("config.json");
        fs::write(&path, v1_with(&owner(3), "pending")).unwrap();
        let before = bytes(&path);
        let rig = Rig::plain(path.clone());
        let ready = rig.collector.wait(0, "ready");

        let listed = rig.channels(1, "h");
        assert_eq!(listed["ok"], true, "{listed}");
        let saved = rig.disk().unwrap();
        let mut expected = DockModel::new(saved.settings.clone());
        expected.channels_listed(
            "h",
            Ok(vec![Named {
                id: "h1".into(),
                name: "voice of h".into(),
            }]),
        );
        assert_eq!(model_fields(&listed), expected.view());
        assert_eq!(listed["settings"], ready["settings"]);
        assert_eq!(bytes(&path), before);
        assert!(!backup(&path).exists());

        let set = rig.set_settings(
            2,
            r#"{"guild_id": "g", "channel_id": "c", "bitrate_kbps": 7000}"#,
        );
        assert_eq!(set["ok"], true, "{set}");
        let disk = rig.disk().unwrap();
        assert_eq!(disk.settings.channel_id.as_deref(), Some("c"));
        assert_eq!(disk.settings.bitrate_kbps, 7000);
        assert_eq!(
            (disk.settings.owned, disk.ownership_certainty),
            (Some(owner(3)), StartCertainty::Pending)
        );
    }

    #[test]
    fn a_failed_request_leaves_its_error_in_the_model() {
        let dir = testdir::fresh("ctl-error-kept");
        let service = FakeService {
            fail: Some(ControlError::NotRunning("no socket".into())),
            ..Default::default()
        };
        let rig = Rig::new::<Unwired>(dir.join("config.json"), service, None);
        let failed = rig.request("dstreamy_status", 1);
        assert_eq!(failed["ok"], false);
        assert!(failed["error"].as_str().unwrap().contains("no socket"));
        let got = rig.request("dstreamy_get_settings", 2);
        assert_eq!(got["ok"], true);
        assert_eq!(model_fields(&got), model_fields(&failed));
        let start = rig.request("dstreamy_start", 3);
        assert_eq!(start["error"], START_UNWIRED);
        let got = rig.request("dstreamy_get_settings", 4);
        assert_eq!(model_fields(&got), model_fields(&start));
        let guilds = rig.request("dstreamy_guilds", 5);
        assert_eq!(guilds["error"], Value::Null);
    }

    #[test]
    fn the_recovery_warning_returns_once_a_success_clears_the_error() {
        let dir = testdir::fresh("ctl-recovery-kept");
        let path = dir.join("config.json");
        fs::write(&path, b"{").unwrap();
        fs::write(backup(&path), v1_with(&owner(3), "pending")).unwrap();
        let rig = Rig::plain(path);
        let ready = rig.collector.wait(0, "ready");
        let warning = ready["error"].clone();
        assert!(warning.as_str().unwrap().contains("config.json"), "{ready}");
        let start = rig.request("dstreamy_start", 1);
        assert_eq!(start["ok"], false);
        let got = rig.request("dstreamy_get_settings", 2);
        assert_eq!(model_fields(&got), model_fields(&start));
        let guilds = rig.request("dstreamy_guilds", 3);
        assert_eq!(guilds["ok"], true);
        assert_eq!(guilds["error"], warning);
    }

    #[test]
    fn a_failed_status_reports_the_error() {
        let dir = testdir::fresh("ctl-status-fail");
        let service = FakeService {
            fail: Some(ControlError::NotRunning("no socket".into())),
            ..Default::default()
        };
        let rig = Rig::new::<Unwired>(dir.join("config.json"), service, None);
        let status = rig.request("dstreamy_status", 1);
        assert_eq!(status["ok"], false);
        assert_eq!(status["service"], Value::Null);
        assert!(status["error"].as_str().unwrap().contains("no socket"));
    }

    #[test]
    fn unload_cancels_a_blocked_wait() {
        let dir = testdir::fresh("ctl-cancel");
        let service = FakeService {
            block: true,
            ..Default::default()
        };
        let mut rig = Rig::new::<Unwired>(dir.join("config.json"), service.clone(), None);
        assert_eq!(
            rig.call("dstreamy_status", &[Arg::Int("request_id", 1)]),
            accepted()
        );
        wait_calls(&service, 1);
        let begun = Instant::now();
        rig.unload();
        assert!(begun.elapsed() < Duration::from_secs(2));
        assert_eq!(rig.collector.count(1), 0);
        assert_eq!(
            rig.call("dstreamy_status", &[Arg::Int("request_id", 2)]),
            refused(SHUTTING_DOWN)
        );
    }

    #[test]
    fn a_cancel_before_any_wait_still_stops() {
        let dir = testdir::fresh("ctl-cancel-early");
        let gate: &'static Gate = Box::leak(Box::new(Gate::new(FAKE_REFS)));
        let handlers = Handlers::new();
        assert!(gate.register(handlers.procs, handlers.signals));
        let handle = spawn::<FakeService, Unwired>(
            gate,
            Parts {
                store: ConfigStore::at(dir.join("config.json")),
                service: FakeService::default(),
                lifecycle: None,
                signals: handlers.signals,
            },
        )
        .unwrap();
        gate.close();
        handle.cancel_unload();
        let begun = Instant::now();
        handle.join();
        assert!(begun.elapsed() < Duration::from_secs(2));
    }

    fn wait_calls(service: &FakeService, n: usize) {
        let deadline = Instant::now() + WAIT;
        loop {
            let calls = service.calls.load(Ordering::SeqCst);
            if calls >= n {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "service saw {calls} calls, expected {n} within {WAIT:?}"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn unload_interrupts_a_held_request_while_a_shutdown_waits_for_it() {
        let dir = testdir::fresh("ctl-shutdown-unload");
        let hold = Arc::new(Semaphore::new(0));
        let service = FakeService {
            hold: Some(hold.clone()),
            ..Default::default()
        };
        let mut rig = Rig::new::<Unwired>(dir.join("config.json"), service.clone(), None);
        assert_eq!(
            rig.call("dstreamy_status", &[Arg::Int("request_id", 1)]),
            accepted()
        );
        wait_calls(&service, 1);
        assert_eq!(
            rig.call("dstreamy_shutdown", &[Arg::Int("request_id", 2)]),
            accepted()
        );
        let watchdog = hold.clone();
        std::thread::spawn(move || {
            std::thread::sleep(WAIT);
            watchdog.add_permits(1);
        });
        let begun = Instant::now();
        rig.unload();
        assert!(begun.elapsed() < Duration::from_secs(2));
        assert_eq!(rig.collector.count(1), 0);
        assert_eq!(rig.collector.count(2), 0);
    }

    #[test]
    fn shutdown_lets_the_current_request_finish_discards_queued_work_and_keeps_the_journal() {
        let base = 0x60_0000;
        let dir = testdir::fresh("ctl-shutdown");
        let path = dir.join("config.json");
        fs::write(&path, v1_with(&owner(3), "pending")).unwrap();
        let before = bytes(&path);
        let hold = Arc::new(Semaphore::new(0));
        let service = FakeService {
            hold: Some(hold.clone()),
            ..Default::default()
        };
        let rig = Rig::new::<Unwired>(path.clone(), service.clone(), None);
        assert_eq!(
            rig.call(
                "dstreamy_stream_starting",
                &[
                    Arg::Int("generation", 1),
                    Arg::Ptr("output", fake_output(base, 0)),
                    Arg::Str("classification", b"unknown")
                ]
            ),
            accepted()
        );
        assert_eq!(
            rig.call("dstreamy_status", &[Arg::Int("request_id", 1)]),
            accepted()
        );
        wait_calls(&service, 1);
        assert_eq!(
            rig.call("dstreamy_get_settings", &[Arg::Int("request_id", 2)]),
            accepted()
        );
        assert_eq!(
            rig.call("dstreamy_shutdown", &[Arg::Int("request_id", 3)]),
            accepted()
        );
        assert_eq!(
            rig.call("dstreamy_get_settings", &[Arg::Int("request_id", 5)]),
            refused(SHUTTING_DOWN)
        );
        hold.add_permits(1);
        let status = rig.collector.wait(1, "status");
        assert_eq!(
            (status["ok"].clone(), status["error"].clone()),
            (json!(true), Value::Null),
            "{status}"
        );
        let queued = rig.collector.wait(2, "get_settings");
        assert_eq!(
            (queued["ok"].clone(), queued["error"].clone()),
            (json!(false), json!(SHUTTING_DOWN))
        );
        let done = rig.collector.wait(3, "shutdown");
        assert_eq!(done["ok"], true);
        assert!(rig.position(1, "status") < rig.position(2, "get_settings"));
        assert!(rig.position(2, "get_settings") < rig.position(3, "shutdown"));
        assert_eq!(held(base), 0);
        assert_eq!(
            rig.call("dstreamy_get_settings", &[Arg::Int("request_id", 4)]),
            refused(SHUTTING_DOWN)
        );
        assert_eq!(bytes(&path), before);
    }

    #[test]
    fn a_subscriber_may_call_a_proc_from_the_signal() {
        let dir = testdir::fresh("ctl-reenter");
        let rig = Rig::plain(dir.join("config.json"));
        let procs = rig.handlers.procs as usize;
        let replies = Arc::new(Mutex::new(Vec::new()));
        let sink = replies.clone();
        let mut once = false;
        *rig.collector.reenter.lock().unwrap() = Some(Box::new(move |v: &Value| {
            if v["request_id"] == 1 && !once {
                once = true;
                let reply = call(
                    procs as *mut _,
                    "dstreamy_get_settings",
                    &[Arg::Int("request_id", 1)],
                );
                sink.lock().unwrap().push(reply);
            }
        }));
        rig.request("dstreamy_get_settings", 1);
        let deadline = Instant::now() + WAIT;
        while rig.collector.count(1) < 2 {
            assert!(
                Instant::now() < deadline,
                "the reentrant request never completed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(*replies.lock().unwrap(), [accepted()]);
    }

    #[test]
    fn stream_observations_emit_unsolicited_state_and_release_refs() {
        let base = 0x70_0000;
        let dir = testdir::fresh("ctl-observe");
        let mut rig = Rig::plain(dir.join("config.json"));
        let observe = |name: &str, g: i64| {
            let mut args = vec![Arg::Int("generation", g)];
            if name != "dstreamy_stream_stopped" {
                args.push(Arg::Ptr("output", fake_output(base, 0)));
                args.push(Arg::Str("classification", b"enhanced"));
            }
            assert_eq!(rig.call(name, &args), accepted());
        };
        observe("dstreamy_stream_starting", 2);
        let seen = rig.collector.wait(0, "stream_starting");
        assert_fields(&seen);
        assert_eq!(
            (
                seen["output"]["generation"].clone(),
                seen["output"]["stream_active"].clone()
            ),
            (json!(2), json!(true))
        );
        observe("dstreamy_stream_stopped", 2);
        let seen = rig.collector.wait(0, "stream_stopped");
        assert_eq!(seen["output"]["stream_active"], false);
        observe("dstreamy_stream_started", 3);
        rig.collector.wait(0, "stream_started");
        assert_eq!(held(base), 1);
        rig.unload();
        assert_eq!(held(base), 0);
        assert!(rig.collector.find(0, "ready").is_some());
    }
}
