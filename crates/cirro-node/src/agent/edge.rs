//! The agent as the edge's [`Router`]: which App a hostname is, and what a
//! request for it waits on first.

use super::*;

/// What the edge does next for a hostname, decided under the VM table's
/// lock and acted on after it is released. Each carries the App's name.
enum Step {
    Answer(Resolution),
    /// The App is starting, waking or parking: hold the request until that
    /// settles.
    Hold(String, Holding),
    /// The App is parked: wake it.
    Wake(String),
}

/// What a held request waits for.
enum Holding {
    /// A start or wake, after which the App runs or the start failed.
    Start,
    /// A park, after which the App is parked and the request wakes it.
    Park,
}

/// The agent as the edge's [`Router`]: a request for a parked App wakes it
/// and is held until it runs, so the client sees latency, not an error.
pub(super) struct Edge(pub(super) Arc<Agent>);

impl Router for Edge {
    fn routes(&self, host: &str) -> bool {
        self.0
            .host_owner(&self.0.vms.lock().unwrap(), host)
            .is_some()
    }

    async fn resolve(&self, host: &str) -> Resolution {
        let agent = &self.0;
        let deadline = tokio::time::Instant::now() + HOLD_FOR_START;
        // Set once this request has held for a wake or tried one. Finding
        // the App parked after that means the wake failed, and the request
        // answers why instead of trying again.
        let mut tried: Option<Option<String>> = None;
        loop {
            // Listening before looking, so a start that settles in between
            // isn't missed.
            let settled = agent.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            match agent.step_for(host) {
                Step::Answer(resolution) => return resolution,
                Step::Hold(name, holding) => {
                    if let Holding::Start = &holding {
                        tried = Some(None);
                    }
                    if tokio::time::timeout_at(deadline, settled).await.is_err() {
                        return Resolution::Unavailable(match holding {
                            Holding::Start => still_starting(&name),
                            Holding::Park => format!(
                                "App {name:?} is still being parked after {HOLD_FOR_START:?}; \
                                 try again shortly"
                            ),
                        });
                    }
                }
                Step::Wake(name) if tried.is_some() => {
                    let why = tried
                        .flatten()
                        .or_else(|| agent.wake_failures.lock().unwrap().get(&name).cloned());
                    return Resolution::Unavailable(why.unwrap_or_else(|| failed_to_wake(&name)));
                }
                Step::Wake(name) => {
                    let woken =
                        tokio::time::timeout_at(deadline, agent.clone().wake_vm(name.clone()));
                    match woken.await {
                        Err(_) => return Resolution::Unavailable(still_starting(&name)),
                        Ok(Ok(_)) => info!(vm = name, host, "woke an App for a request"),
                        // Another request's wake may have got there first,
                        // or the App can't be woken (its snapshot is gone).
                        Ok(Err(ApiError(StatusCode::CONFLICT, why))) => tried = Some(Some(why)),
                        Ok(Err(e)) => return Resolution::Unavailable(wake_failure(&name, &e)),
                    }
                }
            }
        }
    }
}

fn failed_to_wake(name: &str) -> String {
    format!("App {name:?} failed to wake; `cirro logs {name}` may say why")
}

/// Why a wake failed, as the edge's clients may hear it. A conflict is the
/// App's state in the agent's own words. Anything else carries what went
/// wrong inside the Node (the guest's console, the Node's paths), which is
/// for `cirro wake` and the agent's log, not for whoever sent a request.
pub(super) fn wake_failure(name: &str, e: &ApiError) -> String {
    match e {
        ApiError(StatusCode::CONFLICT, why) => why.clone(),
        _ => failed_to_wake(name),
    }
}

fn still_starting(name: &str) -> String {
    format!(
        "App {name:?} is still starting after {HOLD_FOR_START:?}; try again shortly, or see \
         `cirro logs {name}`"
    )
}

impl Agent {
    /// What a request for `host` needs next.
    fn step_for(&self, host: &str) -> Step {
        if let Some(resolution) = self.machines.resolve(host) {
            return Step::Answer(resolution);
        }
        let vms = self.vms.lock().unwrap();
        let Some((name, entry)) = app_with_host(&vms, host) else {
            return Step::Answer(Resolution::NotFound);
        };
        let name = name.clone();
        match entry {
            Entry::Running { traffic, .. } if traffic.parking.load(Ordering::SeqCst) => {
                Step::Hold(name, Holding::Park)
            }
            Entry::Running { info, traffic, .. } => {
                Step::Answer(match (info.vm_address, &info.route) {
                    // Counted under the VM table's lock, so the idle parker
                    // never picks a VM a request is about to reach.
                    (Some(address), Some(route)) => Resolution::Upstream(
                        SocketAddr::new(address.into(), route.port),
                        Lease::new(InFlight::start(traffic)),
                    ),
                    _ => Resolution::Unavailable(format!("App {name:?} has no VM address")),
                })
            }
            Entry::Starting { .. } => Step::Hold(name, Holding::Start),
            Entry::Ended { info, .. } if is_parked(info) => Step::Wake(name),
            Entry::Ended { info, .. } => Step::Answer(Resolution::Unavailable(format!(
                "App {name:?} has ended ({})",
                info.ended.as_ref().map_or("unknown", |e| e.reason.as_str())
            ))),
        }
    }
}
