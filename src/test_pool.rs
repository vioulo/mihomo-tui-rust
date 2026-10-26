use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use super::{Command, Event};
use crate::api::Client;

struct DelayResponder {
    active: Arc<AtomicUsize>,
    max: Arc<AtomicUsize>,
}

impl Respond for DelayResponder {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        let current = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        // Record the high-water mark without a lock.
        let mut observed = self.max.load(Ordering::SeqCst);
        while current > observed {
            match self
                .max
                .compare_exchange(observed, current, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(actual) => observed = actual,
            }
        }
        std::thread::sleep(Duration::from_millis(50));
        self.active.fetch_sub(1, Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_json(serde_json::json!({"delay": 10}))
    }
}

#[tokio::test]
async fn pool_runs_with_bounded_concurrency_and_cancel() {
    let server = MockServer::start().await;
    let active = Arc::new(AtomicUsize::new(0));
    let max = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path_regex(r"^/proxies/.*/delay$"))
        .respond_with(DelayResponder {
            active: active.clone(),
            max: max.clone(),
        })
        .mount(&server)
        .await;

    let client = Arc::new(Client::new(&server.uri(), None).unwrap());
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    crate::spawn_test_pool(client, 4, command_rx, event_tx);

    let names: Vec<String> = (0..20).map(|index| format!("n{index}")).collect();
    command_tx
        .send(Command::TestMany {
            generation: 1,
            names: names.clone(),
            url: "http://x/204".to_string(),
            timeout_ms: 5000,
        })
        .unwrap();

    let mut progress = 0usize;
    let mut finished = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && !finished {
        match tokio::time::timeout(Duration::from_millis(50), event_rx.recv()).await {
            Ok(Some(Event::TestProgress { .. })) => {
                progress += 1;
                if progress >= 3 {
                    let _ = command_tx.send(Command::CancelTests(1));
                }
            }
            Ok(Some(Event::TestsFinished { generation: 1, .. })) => {
                finished = true;
            }
            _ => {}
        }
    }

    assert!(finished, "TestsFinished was emitted");
    assert!(
        max.load(Ordering::SeqCst) <= 4,
        "concurrency exceeded the limit: {}",
        max.load(Ordering::SeqCst)
    );
    assert!(
        progress < 20,
        "cancellation should have stopped some requests: {progress}"
    );
}
