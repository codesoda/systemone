//! Resident backend workers with bounded in-memory admission.
//!
//! Each enabled backend loads once onto its own owner thread. Requests are
//! admitted through a process-wide semaphore and a per-instance bounded
//! queue, then awaited asynchronously. One permit follows a job from
//! admission until the host actually finishes, even if the caller's deadline
//! expired, so a noninterruptible native call never overlaps the next.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use systemone_core::{
    Backend, BackendDescription, BackendId, CallContext, Capabilities, DecisionHost,
    DecisionRequest, DecisionResponse, HostError,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

/// A configured instance ready to be loaded into the registry.
pub struct RegistryEntry {
    pub backend: Arc<dyn Backend>,
    pub queue_capacity: usize,
    pub max_in_flight: usize,
}

/// Successful evaluation with the routing evidence the service exposes.
#[derive(Clone, Debug)]
pub struct Evaluated {
    pub backend: BackendId,
    pub response: DecisionResponse,
    pub elapsed: Duration,
}

pub struct Registry {
    workers: BTreeMap<BackendId, Worker>,
    /// Descriptions of every configured instance, including disabled ones.
    descriptions: Vec<BackendDescription>,
    default_backend: Option<BackendId>,
    admission: Arc<Semaphore>,
    request_sequence: AtomicU64,
    shutting_down: Arc<AtomicBool>,
}

impl Registry {
    /// Load every entry. Fails if any enabled backend cannot load; already
    /// loaded workers are shut down first.
    pub fn load(
        entries: Vec<RegistryEntry>,
        descriptions: Vec<BackendDescription>,
        default_backend: Option<BackendId>,
        max_admitted_jobs: usize,
    ) -> Result<Self, HostError> {
        if entries.is_empty() {
            return Err(HostError::validation("no enabled backends to load"));
        }
        let shutting_down = Arc::new(AtomicBool::new(false));
        let mut workers = BTreeMap::new();
        for entry in entries {
            if entry.max_in_flight != 1 {
                return Err(HostError::validation(format!(
                    "backends.{}.max_in_flight must be 1: v1 runs one resident worker per instance",
                    entry.backend.id()
                )));
            }
            let id = entry.backend.id().clone();
            match Worker::spawn(
                entry.backend,
                entry.queue_capacity,
                Arc::clone(&shutting_down),
            ) {
                Ok(worker) => {
                    workers.insert(id, worker);
                }
                Err(error) => {
                    shutting_down.store(true, Ordering::Release);
                    for (_, worker) in workers {
                        let _ = worker.shutdown();
                    }
                    return Err(HostError::unavailable(format!(
                        "backend {id} failed to load: {error}"
                    )));
                }
            }
        }
        if let Some(default) = &default_backend
            && !workers.contains_key(default)
        {
            return Err(HostError::validation(format!(
                "default_backend {default} is not an enabled backend"
            )));
        }
        Ok(Self {
            workers,
            descriptions,
            default_backend,
            admission: Arc::new(Semaphore::new(max_admitted_jobs)),
            request_sequence: AtomicU64::new(1),
            shutting_down,
        })
    }

    #[must_use]
    pub fn default_backend(&self) -> Option<&BackendId> {
        self.default_backend.as_ref()
    }

    #[must_use]
    pub fn descriptions(&self) -> &[BackendDescription] {
        &self.descriptions
    }

    pub fn loaded(&self) -> impl Iterator<Item = (&BackendId, &Capabilities)> {
        self.workers
            .iter()
            .map(|(id, worker)| (id, &worker.capabilities))
    }

    #[must_use]
    pub fn capabilities(&self, id: &BackendId) -> Option<&Capabilities> {
        self.workers.get(id).map(|worker| &worker.capabilities)
    }

    #[must_use]
    pub fn is_ready(&self, id: &BackendId) -> bool {
        self.workers
            .get(id)
            .is_some_and(|worker| worker.ready.load(Ordering::Acquire))
    }

    /// All loaded backends are ready and no shutdown has begun.
    #[must_use]
    pub fn all_ready(&self) -> bool {
        !self.shutting_down.load(Ordering::Acquire)
            && self
                .workers
                .values()
                .all(|worker| worker.ready.load(Ordering::Acquire))
    }

    /// Resolve an explicit selector or the default. Unknown/disabled
    /// instances are errors; nothing is implied from the model name.
    pub fn select(&self, selector: Option<&str>) -> Result<BackendId, HostError> {
        match selector {
            Some(selector) => {
                let id = BackendId::new(selector)?;
                if self.workers.contains_key(&id) {
                    Ok(id)
                } else if self.descriptions.iter().any(|d| d.id == id) {
                    Err(HostError::validation(format!(
                        "backend {id} is configured but not enabled"
                    )))
                } else {
                    Err(HostError::validation(format!("unknown backend {id}")))
                }
            }
            None => self.default_backend.clone().ok_or_else(|| {
                HostError::validation(
                    "no default_backend is configured; select a backend explicitly",
                )
            }),
        }
    }

    pub fn next_request_id(&self) -> u64 {
        self.request_sequence.fetch_add(1, Ordering::Relaxed)
    }

    /// Admit, queue and await one request on `backend`.
    pub async fn evaluate(
        &self,
        backend: &BackendId,
        request: DecisionRequest,
        request_id: String,
        deadline: Instant,
    ) -> Result<Evaluated, HostError> {
        let started = Instant::now();
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(HostError::unavailable("service is shutting down"));
        }
        let worker = self
            .workers
            .get(backend)
            .ok_or_else(|| HostError::validation(format!("unknown backend {backend}")))?;
        if !worker.ready.load(Ordering::Acquire) {
            return Err(HostError::unavailable(format!(
                "backend {backend} is not ready"
            )));
        }
        worker.capabilities.check(&request)?;
        worker
            .capabilities
            .resolve_model(request.model.as_deref())?;
        let permit = Arc::clone(&self.admission)
            .try_acquire_owned()
            .map_err(|_| HostError::Overloaded)?;
        let context = CallContext::new(request_id, Some(deadline));
        let cancellation = context.cancellation();
        let mut guard = CancellationGuard {
            flag: cancellation,
            armed: true,
        };
        let (reply, receiver) = oneshot::channel();
        let job = Job {
            request,
            context,
            reply,
            _permit: permit,
        };
        match worker.sender.try_send(Command::Infer(Box::new(job))) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(HostError::Overloaded),
            Err(TrySendError::Disconnected(_)) => {
                worker.ready.store(false, Ordering::Release);
                return Err(HostError::unavailable(format!(
                    "backend {backend} worker is unavailable"
                )));
            }
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(HostError::Timeout)?;
        let result = tokio::time::timeout(remaining, receiver).await;
        if result.is_ok() {
            guard.armed = false;
        }
        match result {
            Err(_) => Err(HostError::Timeout),
            Ok(Err(_)) => {
                worker.ready.store(false, Ordering::Release);
                Err(HostError::unavailable(format!(
                    "backend {backend} worker stopped before replying"
                )))
            }
            Ok(Ok(Err(error))) => {
                if error.is_terminal() {
                    worker.ready.store(false, Ordering::Release);
                }
                Err(error)
            }
            Ok(Ok(Ok(response))) => Ok(Evaluated {
                backend: backend.clone(),
                response,
                elapsed: started.elapsed(),
            }),
        }
    }

    /// Stop admission and join every worker. Errors from individual hosts
    /// are collected; the first is returned.
    pub fn shutdown(self) -> Result<(), HostError> {
        self.shutting_down.store(true, Ordering::Release);
        let mut first_error = None;
        for (id, worker) in self.workers {
            if let Err(error) = worker.shutdown() {
                tracing::error!(backend = %id, error = %error, "backend shutdown failed");
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Begin shutdown without joining (used by the signal handler).
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        for worker in self.workers.values() {
            worker.ready.store(false, Ordering::Release);
        }
    }
}

struct CancellationGuard {
    flag: Arc<AtomicBool>,
    armed: bool,
}

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.flag.store(true, Ordering::Release);
        }
    }
}

struct Job {
    request: DecisionRequest,
    context: CallContext,
    reply: oneshot::Sender<Result<DecisionResponse, HostError>>,
    _permit: OwnedSemaphorePermit,
}

enum Command {
    Infer(Box<Job>),
    Shutdown,
}

struct Worker {
    sender: SyncSender<Command>,
    join: Option<JoinHandle<Result<(), HostError>>>,
    ready: Arc<AtomicBool>,
    capabilities: Capabilities,
}

impl Worker {
    fn spawn(
        backend: Arc<dyn Backend>,
        queue_capacity: usize,
        shutting_down: Arc<AtomicBool>,
    ) -> Result<Self, HostError> {
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
        let ready = Arc::new(AtomicBool::new(false));
        let ready_for_thread = Arc::clone(&ready);
        let name = format!("systemone-{}", backend.id());
        let join = std::thread::Builder::new()
            .name(name)
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_main(
                        backend.as_ref(),
                        &receiver,
                        &startup_sender,
                        &ready_for_thread,
                        &shutting_down,
                    )
                }));
                ready_for_thread.store(false, Ordering::Release);
                match result {
                    Ok(result) => result,
                    Err(_) => {
                        tracing::error!("backend owner thread panicked");
                        Err(HostError::unavailable("backend owner thread panicked"))
                    }
                }
            })
            .map_err(|error| HostError::internal(error.to_string()))?;
        let capabilities = match startup_receiver.recv() {
            Ok(Ok(capabilities)) => capabilities,
            Ok(Err(error)) => {
                let _ = join_worker(join);
                return Err(error);
            }
            Err(_) => {
                return match join_worker(join) {
                    Err(error) => Err(error),
                    Ok(()) => Err(HostError::unavailable(
                        "backend owner thread stopped during startup",
                    )),
                };
            }
        };
        Ok(Self {
            sender,
            join: Some(join),
            ready,
            capabilities,
        })
    }

    fn shutdown(mut self) -> Result<(), HostError> {
        self.ready.store(false, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = self.sender.send(Command::Shutdown);
            return join_worker(join);
        }
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.ready.store(false, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = self.sender.send(Command::Shutdown);
            let _ = join.join();
        }
    }
}

fn join_worker(join: JoinHandle<Result<(), HostError>>) -> Result<(), HostError> {
    join.join()
        .map_err(|_| HostError::unavailable("backend owner thread panicked"))?
}

fn worker_main(
    backend: &dyn Backend,
    receiver: &Receiver<Command>,
    startup: &SyncSender<Result<Capabilities, HostError>>,
    ready: &AtomicBool,
    shutting_down: &AtomicBool,
) -> Result<(), HostError> {
    let mut host: Box<dyn DecisionHost> = match backend.load() {
        Ok(host) => host,
        Err(error) => {
            let _ = startup.send(Err(error));
            return Ok(());
        }
    };
    ready.store(true, Ordering::Release);
    if startup.send(Ok(host.capabilities().clone())).is_err() {
        ready.store(false, Ordering::Release);
        return host.shutdown();
    }
    while let Ok(command) = receiver.recv() {
        match command {
            Command::Shutdown => break,
            Command::Infer(job) => {
                let Job {
                    request,
                    context,
                    reply,
                    _permit,
                } = *job;
                let result = if shutting_down.load(Ordering::Acquire) {
                    Err(HostError::unavailable("service is shutting down"))
                } else if reply.is_closed() {
                    Err(HostError::Cancelled)
                } else {
                    context
                        .check()
                        .and_then(|()| host.evaluate(&request, &context))
                };
                let terminal = matches!(&result, Err(error) if error.is_terminal());
                if terminal {
                    ready.store(false, Ordering::Release);
                }
                let _ = reply.send(result);
                drop(_permit);
                if terminal {
                    break;
                }
            }
        }
    }
    ready.store(false, Ordering::Release);
    let result = host.shutdown();
    if let Err(error) = &result {
        tracing::error!(error = %error, "host shutdown failed");
    }
    result
}
