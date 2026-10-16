use crate::config::{FakeConfig, Scenario};
use tokio::sync::{mpsc, watch};
use tokio::time::{Duration, sleep};

pub const UPDATE_CAPACITY: usize = 8;
pub const MAX_DELTA_BYTES: usize = 128;
pub const UNDURABLE_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub enum Update {
    Delta(String),
    Finished,
    Failed(String),
    ToolStarted,
    ToolFinished,
    Steered,
}

// Deterministic transport fixture; it never invokes tools or a network endpoint.
pub async fn run(config: FakeConfig, sender: mpsc::Sender<Update>, cancel: watch::Receiver<bool>) {
    let (_steer, steering) = watch::channel(false);
    run_controlled(config, sender, cancel, steering).await;
}

pub async fn run_controlled(
    config: FakeConfig,
    sender: mpsc::Sender<Update>,
    mut cancel: watch::Receiver<bool>,
    mut steer: watch::Receiver<bool>,
) {
    async fn requested(receiver: &mut watch::Receiver<bool>) {
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
    let work = async {
        let wait = if config.scenario == Scenario::Waiting {
            config.first_event_ms.max(5000)
        } else {
            config.first_event_ms
        };
        tokio::select! {
            _ = sleep(Duration::from_millis(wait)) => {},
            _ = requested(&mut steer) => { let _ = sender.send(Update::Steered).await; return; }
        }
        if config.scenario == Scenario::Error {
            let _ = sender
                .send(Update::Failed(
                    "fixture: request failed before first event".into(),
                ))
                .await;
            return;
        }
        let chunks = if config.scenario == Scenario::Burst {
            config.chunks
        } else {
            24
        };
        for i in 0..chunks {
            if *steer.borrow() {
                let _ = sender.send(Update::Steered).await;
                return;
            }
            // A local deterministic fixture boundary, never a shell or API call.
            // Steer is deliberately not selected during the simulated tool wait.
            if i == 4 && config.scenario != Scenario::Burst {
                if sender.send(Update::ToolStarted).await.is_err() {
                    return;
                }
                sleep(Duration::from_millis(config.delay_ms)).await;
                if sender.send(Update::ToolFinished).await.is_err() {
                    return;
                }
                if *steer.borrow() {
                    let _ = sender.send(Update::Steered).await;
                    return;
                }
            }
            if config.scenario == Scenario::Interrupted && i == 8 {
                let _ = sender
                    .send(Update::Failed(
                        "fixture: stream interrupted; partial response saved".into(),
                    ))
                    .await;
                return;
            }
            let text = if config.scenario == Scenario::Burst {
                format!("fixture block {i:06}: bounded streaming output.\n")
            } else if i == 0 {
                "[test provider] No API or tools are connected yet.\n".into()
            } else {
                format!("Streaming chunk {i:02}. ")
            };
            assert!(
                text.len() <= MAX_DELTA_BYTES,
                "fixture delta exceeds its byte budget"
            );
            if sender.send(Update::Delta(text)).await.is_err() {
                return;
            }
            if config.scenario != Scenario::Burst {
                tokio::select! {
                    _ = sleep(Duration::from_millis(config.delay_ms)) => {},
                    _ = requested(&mut steer) => { let _ = sender.send(Update::Steered).await; return; }
                }
            }
        }
        let _ = sender.send(Update::Finished).await;
    };
    tokio::select! {
        _ = work => {},
        _ = async { while !*cancel.borrow_and_update() { if cancel.changed().await.is_err() { break; } } } => {},
    }
}
