use crate::horust::bus::BusConnector;
use crate::horust::formats::{
    Event, ExitStatus, FailureStrategy, HealthinessStatus, RestartStrategy, Service, ServiceName,
    ServiceStatus,
};
use crate::horust::healthcheck;
use nix::sys::signal;
use nix::unistd;
use repo::Repo;
use service_handler::ServiceHandler;
use std::fmt::Debug;
use std::ops::Mul;
use std::thread;
use std::time::{Duration, Instant};

mod process_spawner;
mod reaper;
mod repo;
mod service_handler;

pub(crate) mod signal_handling;

// Spawns and runs this component in a new thread.
pub fn spawn(
    bus: BusConnector<Event>,
    services: Vec<Service>,
) -> std::thread::JoinHandle<ExitStatus> {
    thread::spawn(move || Runtime::new(bus, services).run())
}

#[derive(Debug)]
pub struct Runtime {
    is_shutting_down: bool,
    repo: Repo,
}

impl Runtime {
    fn new(bus: BusConnector<Event>, services: Vec<Service>) -> Self {
        let repo = Repo::new(bus, services);
        Self {
            repo,
            is_shutting_down: false,
        }
    }

    /// Generates events that, if applied, will make service_handler FSM progress
    fn next(&self, service_handler: &ServiceHandler) -> Vec<Event> {
        if self.is_shutting_down {
            next_events_shutting_down(service_handler)
        } else {
            self.next_events(service_handler)
        }
    }

    /// Generate the events needed for moving forward the FSM for the service handler
    /// If the system is shutting down, it will call next_shutting_down.
    fn next_events(&self, service_handler: &ServiceHandler) -> Vec<Event> {
        let ev_status =
            |status: ServiceStatus| Event::new_status_changed(service_handler.name(), status);
        let vev_status = |status: ServiceStatus| vec![ev_status(status)];
        match service_handler.status {
            ServiceStatus::Initial if self.repo.is_service_runnable(&service_handler) => {
                vec![Event::Run(service_handler.name().clone())]
            }
            ServiceStatus::Started if service_handler.healthiness_checks_failed == 0 => {
                vev_status(ServiceStatus::Running)
            }
            // If 2 healthcheks are failed, then kill the service. Maybe this should be parametrized
            ServiceStatus::Running if service_handler.healthiness_checks_failed > 2 => vec![
                ev_status(ServiceStatus::InKilling),
                Event::Kill(service_handler.name().clone()),
            ],
            ServiceStatus::Success => {
                vec![handle_restart_strategy(service_handler.service(), false)]
            }
            ServiceStatus::Failed => {
                let mut failure_evs = handle_failed_service(
                    self.repo.get_dependents(service_handler.name()),
                    service_handler.service(),
                );
                let other_services_termination = self
                    .repo
                    .get_die_if_failed(service_handler.name())
                    .into_iter()
                    .map(|sh_name| {
                        vec![
                            Event::new_status_changed(sh_name, ServiceStatus::InKilling),
                            Event::Kill(sh_name.clone()),
                        ]
                    })
                    .flatten();

                let service_ev = handle_restart_strategy(service_handler.service(), true);

                failure_evs.push(service_ev);
                failure_evs.extend(other_services_termination);
                failure_evs
            }
            ServiceStatus::InKilling if should_force_kill(service_handler) => vec![
                Event::new_force_kill(service_handler.name()),
                Event::new_status_changed(service_handler.name(), ServiceStatus::Failed),
            ],

            _ => vec![],
        }
    }

    /// Handle the events, returns Events (state changes) to be dispatched.
    fn handle_event(&mut self, ev: Event) -> Vec<Event> {
        match ev {
            Event::ServiceExited(service_name, exit_code) => {
                let pid = self.repo.get_sh(&service_name).pid.unwrap();
                self.repo.remove_pid(pid);
                let service_handler = self.repo.get_mut_sh(&service_name);
                service_handler.shutting_down_start = None;
                service_handler.pid = None;

                let has_failed = !service_handler
                    .service()
                    .failure
                    .successful_exit_code
                    .contains(&exit_code);
                let healthcheck_failed = service_handler.healthiness_checks_failed > 0
                    && service_handler.status == ServiceStatus::Running;
                service_handler.status = if has_failed || healthcheck_failed {
                    warn!(
                        "Service: {} has failed, exit code: {}, healthchecks: {}",
                        service_handler.name(),
                        exit_code,
                        healthcheck_failed
                    );

                    // If it has failed too quickly, increase service_handler's restart attempts
                    // and check if it has more attempts left.
                    let early_states = vec![
                        ServiceStatus::Initial,
                        ServiceStatus::Starting,
                        ServiceStatus::Started,
                    ];
                    if early_states.contains(&service_handler.status) {
                        service_handler.restart_attempts += 1;
                        if service_handler.restart_attempts_are_over() {
                            //Game over!
                            ServiceStatus::FinishedFailed
                        } else {
                            ServiceStatus::Initial
                        }
                    } else {
                        // If wasn't starting, then it's just failed in a usual way:
                        ServiceStatus::Failed
                    }
                } else {
                    info!(
                        "Service: {} successfully exited with: {}.",
                        service_handler.name(),
                        exit_code
                    );
                    ServiceStatus::Success
                };
                debug!("New state for exited service: {:?}", service_handler.status);
                vec![Event::StatusChanged(
                    service_name.clone(),
                    service_handler.status.clone(),
                )]
            }
            Event::Run(service_name) if self.repo.get_sh(&service_name).is_initial() => {
                let mut evs = vec![];
                let service_handler = self.repo.get_mut_sh(&service_name);
                evs.push(Event::StatusChanged(service_name, ServiceStatus::Starting));
                service_handler.status = ServiceStatus::Starting;
                let res = healthcheck::prepare_service(&service_handler.service().healthiness);
                if res.is_err() {
                    //TODO: maybe this is a bit too aggressive.
                    error!(
                        "Prepare healthiness checks failed for service: {}, shutting down...",
                        service_handler.name()
                    );
                    service_handler.status = ServiceStatus::FinishedFailed;
                    self.is_shutting_down = true;
                    return vec![
                        Event::StatusChanged(
                            service_handler.name().clone(),
                            ServiceStatus::FinishedFailed,
                        ),
                        Event::ShuttingDownInitiated,
                    ];
                }
                let backoff = service_handler
                    .service()
                    .restart
                    .backoff
                    .mul(service_handler.restart_attempts);
                process_spawner::spawn_fork_exec_handler(
                    service_handler.service().clone(),
                    backoff,
                    self.repo.bus.clone(),
                );
                evs
            }
            Event::SpawnFailed(s_name) => {
                let service_handler = self.repo.get_mut_sh(&s_name);
                service_handler.status = ServiceStatus::Failed;
                vec![Event::StatusChanged(s_name, ServiceStatus::Failed)]
            }
            Event::Kill(service_name) => {
                debug!("Received kill request");
                let service_handler = self.repo.get_mut_sh(&service_name);
                if service_handler.is_in_killing() {
                    service_handler.shutting_down_started();
                    kill(service_handler, None);
                } else {
                    debug!(
                        "Cannot send kill request, service was in: {}",
                        service_handler.status
                    );
                }
                vec![]
            }
            Event::ForceKill(service_name) if self.repo.get_sh(&service_name).is_in_killing() => {
                debug!("Going to forcekill {}", service_name);
                let service_handler = self.repo.get_mut_sh(&service_name);
                kill(&service_handler, Some(signal::SIGKILL));
                service_handler.status = ServiceStatus::Failed;
                vec![Event::new_status_changed(
                    service_handler.name(),
                    ServiceStatus::Failed,
                )]
            }
            Event::PidChanged(service_name, pid) => {
                self.repo.add_pid(pid, service_name.clone());

                let service_handler = self.repo.get_mut_sh(&service_name);
                service_handler.pid = Some(pid);
                if service_handler.is_in_killing() {
                    // Ah! Gotcha!
                    service_handler.shutting_down_start = Some(Instant::now());
                    kill(service_handler, None)
                } else {
                    service_handler.status = ServiceStatus::Started;
                    return vec![Event::StatusChanged(service_name, ServiceStatus::Started)];
                }

                vec![]
            }
            Event::HealthCheck(s_name, health) => {
                let sh = self.repo.get_mut_sh(&s_name);
                // Count the failed healthiness checks. The state change producer wll handle states
                // changes (if they're needed)
                if vec![
                    ServiceStatus::Running,
                    ServiceStatus::Started,
                    ServiceStatus::Starting,
                ]
                .contains(&sh.status)
                {
                    if let HealthinessStatus::Healthy = health {
                        sh.healthiness_checks_failed = 0;
                    } else {
                        sh.healthiness_checks_failed += 1;
                    }
                };
                vec![]
            }
            Event::ShuttingDownInitiated => {
                self.is_shutting_down = true;
                vec![]
            }
            ev => {
                trace!("ignoring: {:?}", ev);
                vec![]
            }
        }
    }

    /// Blocking call.
    /// This function will run the services and reap dead pids.
    fn run(mut self) -> ExitStatus {
        while !self.repo.all_have_finished() {
            // Ingest updates
            let events = self.repo.get_events();
            debug!("Applying events... {:?}", events);
            if signal_handling::is_sigterm_received() && !self.is_shutting_down {
                self.repo.send_ev(Event::ShuttingDownInitiated);
            }
            let produced_evs: Vec<Event> = events
                .into_iter()
                .map(|ev| self.handle_event(ev))
                .flatten()
                .collect();
            let next_evs: Vec<Event> = self
                .repo
                .services
                .iter()
                .map(|(_s_name, sh)| self.next(sh))
                .flatten()
                .chain(reaper::run(&self.repo))
                .collect();
            next_evs.iter().for_each(|ev| {
                if let Event::StatusChanged(s_name, new_status) = ev {
                    let new_sh = handle_status_changed_event(
                        self.repo.services.remove(s_name).unwrap(),
                        new_status,
                    );
                    self.repo.services.insert(s_name.clone(), new_sh);
                }
            });
            produced_evs
                .into_iter()
                .chain(next_evs)
                .for_each(|ev| self.repo.send_ev(ev));
            std::thread::sleep(Duration::from_millis(300));
        }

        debug!("All services have finished");
        // If we're the init system, let's be sure that everything stops before exiting.
        let init_pid = unistd::Pid::from_raw(1);
        // TODO: Test (probably via docker).
        if unistd::getpid() == init_pid {
            let all_processes = unistd::Pid::from_raw(-1);
            let _res = signal::kill(all_processes, signal::SIGTERM);
            thread::sleep(Duration::from_secs(3));
            let _res = signal::kill(all_processes, signal::SIGKILL);
        }

        self.repo.send_ev(Event::ShuttingDownInitiated);
        if self.repo.any_finished_failed() {
            ExitStatus::SomeServiceFailed
        } else {
            ExitStatus::Successful
        }
    }
}

// TODO: test
/// Handles the status changed event
fn handle_status_changed_event(
    service_handler: ServiceHandler,
    new_status: &ServiceStatus,
) -> ServiceHandler {
    // A -> [B,C] means that transition to A is allowed only if service is in state B or C.
    let allowed_transitions = hashmap! {
        ServiceStatus::Initial        => vec![ServiceStatus::Success, ServiceStatus::Failed],
        ServiceStatus::Started        => vec![ServiceStatus::Starting],
        ServiceStatus::InKilling      => vec![ServiceStatus::Initial,
                                              ServiceStatus::Running,
                                              ServiceStatus::Starting,
                                              ServiceStatus::Started],
        ServiceStatus::Running        => vec![ServiceStatus::Started],
        ServiceStatus::FinishedFailed => vec![ServiceStatus::Failed, ServiceStatus::InKilling],
        ServiceStatus::Success        => vec![ServiceStatus::Starting,
                                              ServiceStatus::Started,
                                              ServiceStatus::Running,
                                              ServiceStatus::InKilling],
        ServiceStatus::Failed         => vec![ServiceStatus::Starting,
                                              ServiceStatus::Started,
                                              ServiceStatus::Running,
                                              ServiceStatus::InKilling],
        ServiceStatus::Finished       => vec![ServiceStatus::Success,
                                             ServiceStatus::Initial],
    };
    let allowed = allowed_transitions.get(&new_status).unwrap();
    let mut new_sh = service_handler.clone();
    if allowed.contains(&service_handler.status) {
        match new_status {
            ServiceStatus::Started if allowed.contains(&service_handler.status) => {
                new_sh.status = ServiceStatus::Started;
                new_sh.restart_attempts = 0;
            }
            ServiceStatus::Running if allowed.contains(&service_handler.status) => {
                new_sh.status = ServiceStatus::Running;
                new_sh.healthiness_checks_failed = 0;
            }
            ServiceStatus::InKilling if allowed.contains(&service_handler.status) => {
                debug!(
                    " service: {},  status: {}, new status: {}",
                    service_handler.name(),
                    service_handler.status,
                    new_status
                );
                if service_handler.status == ServiceStatus::Initial {
                    new_sh.status = ServiceStatus::Success;
                } else {
                    new_sh.status = ServiceStatus::InKilling;
                }
            }
            new_status => {
                new_sh.status = new_status.clone();
            }
        }
    } else {
        debug!(
            "Tried to make an illegal transition: (current) {} -> {} (received) for service: {}",
            service_handler.status,
            new_status,
            service_handler.name()
        );
    }
    new_sh
}

/// This next function assumes that the system is shutting down.
/// It will make progress in the direction of shutting everything down.
fn next_events_shutting_down(service_handler: &ServiceHandler) -> Vec<Event> {
    let ev_status =
        |status: ServiceStatus| Event::new_status_changed(service_handler.name(), status);
    let vev_status = |status: ServiceStatus| vec![ev_status(status)];

    // Handle the new state separately if we're shutting down.
    match service_handler.status {
        ServiceStatus::Running | ServiceStatus::Started => vec![
            ev_status(ServiceStatus::InKilling),
            Event::Kill(service_handler.name().clone()),
        ],
        ServiceStatus::Success | ServiceStatus::Initial => vev_status(ServiceStatus::Finished),
        ServiceStatus::Failed => vev_status(ServiceStatus::FinishedFailed),
        ServiceStatus::InKilling if should_force_kill(service_handler) => {
            vec![Event::new_force_kill(service_handler.name())]
        }
        _ => vec![],
    }
}

/// Produce events based on the Restart Strategy of the service.
fn handle_restart_strategy(service: &Service, is_failed: bool) -> Event {
    let new_status = |status| Event::new_status_changed(&service.name, status);
    let ev = match service.restart.strategy {
        RestartStrategy::Never if is_failed => new_status(ServiceStatus::FinishedFailed),
        RestartStrategy::OnFailure if is_failed => new_status(ServiceStatus::Initial),
        RestartStrategy::Never | RestartStrategy::OnFailure => new_status(ServiceStatus::Finished),
        RestartStrategy::Always => new_status(ServiceStatus::Initial),
    };
    debug!("Restart strategy applied, ev: {:?}", ev);
    ev
}

/// This is applied to both failed and FinishedFailed services.
fn handle_failed_service(deps: Vec<ServiceName>, failed_sh: &Service) -> Vec<Event> {
    match failed_sh.failure.strategy {
        FailureStrategy::Shutdown => vec![Event::ShuttingDownInitiated],
        FailureStrategy::KillDependents => {
            debug!("Failed service has kill-dependents strategy, going to mark them all..");
            deps.iter()
                .map(|sh| {
                    vec![
                        Event::new_status_changed(sh, ServiceStatus::InKilling),
                        Event::Kill(sh.clone()),
                    ]
                })
                .flatten()
                .collect()
        }
        FailureStrategy::Ignore => vec![],
    }
}

/// Check if we've waitied enough for the service to exit
fn should_force_kill(service_handler: &ServiceHandler) -> bool {
    if service_handler.pid.is_none() {
        // Since it was in the started state, it doesn't have a pid yet.
        // Let's give it the time to start and exit.
        return false;
    }
    if let Some(shutting_down_elapsed_secs) = service_handler.shutting_down_start {
        let shutting_down_elapsed_secs = shutting_down_elapsed_secs.elapsed().as_secs();
        debug!(
            "{}, should not force kill. Elapsed: {}, termination wait: {}",
            service_handler.name(),
            shutting_down_elapsed_secs,
            service_handler.service().termination.wait.clone().as_secs()
        );
        shutting_down_elapsed_secs > service_handler.service().termination.wait.clone().as_secs()
    } else {
        // this might happen, because InKilling state is emitted before the Kill event.
        // So maybe the runtime has received only the InKilling state change, but hasn't sent the
        // signal yet. So it should be fine.
        debug!("There is no shutting down elapsed secs.");
        false
    }
}

/// Kill wrapper, will send signal to sh and handles the result.
/// By default it will send the signal defined in the termination section of the service.
fn kill(sh: &ServiceHandler, signal: Option<signal::Signal>) {
    let signal = signal.unwrap_or_else(|| sh.service().termination.signal.into());
    debug!("Going to send {} signal to pid {:?}", signal, sh.pid());
    if let Some(pid) = sh.pid() {
        if let Err(error) = signal::kill(pid, signal) {
            match error.as_errno().expect("errno empty!") {
                // No process or process group can be found corresponding to that specified by pid
                // It has exited already, so it's fine.
                nix::errno::Errno::ESRCH => (),
                _ => error!(
                    "Error killing the process: {}, service: {}, pid: {:?}",
                    error,
                    sh.name(),
                    pid,
                ),
            }
        }
    } else {
        warn!(
            "{}: Missing pid to kill but service was in {:?} state.",
            sh.name(),
            sh.status
        );
    }
}

#[cfg(test)]
mod test {
    use crate::horust::formats::{FailureStrategy, Service, ServiceStatus};
    use crate::horust::runtime::service_handler::ServiceHandler;
    use crate::horust::runtime::{
        handle_failed_service, handle_restart_strategy, should_force_kill,
    };
    use crate::horust::Event;
    use nix::unistd::Pid;
    use std::ops::Sub;
    use std::time::Duration;
    #[test]
    fn test_handle_restart_strategy() {
        let new_status = |status| Event::new_status_changed(&"servicename".to_string(), status);
        let matrix = vec![
            (false, "always", new_status(ServiceStatus::Initial)),
            (true, "always", new_status(ServiceStatus::Initial)),
            (true, "on-failure", new_status(ServiceStatus::Initial)),
            (false, "on-failure", new_status(ServiceStatus::Finished)),
            (true, "never", new_status(ServiceStatus::FinishedFailed)),
            (false, "never", new_status(ServiceStatus::Finished)),
        ];
        matrix
            .into_iter()
            .for_each(|(has_failed, strategy, expected)| {
                let service = format!(
                    r#"name="servicename"
command = "Not relevant"
[restart]
strategy = "{}"
"#,
                    strategy
                );
                let service: Service = toml::from_str(service.as_str()).unwrap();
                let received = handle_restart_strategy(&service, has_failed);
                assert_eq!(received, expected);
            });
    }

    #[test]
    fn test_should_force_kill() {
        let service = r#"command="notrelevant"
[termination]
wait = "10s"
"#;
        let service: Service = toml::from_str(service).unwrap();
        let mut sh: ServiceHandler = service.into();
        assert!(!should_force_kill(&sh));
        sh.shutting_down_started();
        sh.status = ServiceStatus::InKilling;
        assert!(!should_force_kill(&sh));
        let old_start = sh.shutting_down_start;
        let past_wait = Some(sh.shutting_down_start.unwrap().sub(Duration::from_secs(20)));
        sh.shutting_down_start = past_wait;
        assert!(!should_force_kill(&sh));
        sh.pid = Some(Pid::this());
        sh.shutting_down_start = old_start;
        assert!(!should_force_kill(&sh));
        sh.shutting_down_start = past_wait;
        assert!(should_force_kill(&sh));
    }

    #[test]
    fn test_handle_failed_service() {
        let mut service = Service::from_name("b");
        let evs = handle_failed_service(vec!["a".into()], &service.clone());
        assert!(evs.is_empty());

        service.failure.strategy = FailureStrategy::KillDependents;
        let evs = handle_failed_service(vec!["a".into()], &service.clone());
        let exp = vec![
            Event::new_status_changed(&"a".to_string(), ServiceStatus::InKilling),
            Event::Kill("a".into()),
        ];
        assert_eq!(evs, exp);

        service.failure.strategy = FailureStrategy::Shutdown;
        let evs = handle_failed_service(vec!["a".into()], &service.into());
        let exp = vec![Event::ShuttingDownInitiated];
        assert_eq!(evs, exp);
    }
}
