use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use tokio::sync::oneshot;

use super::*;

fn pool(running: usize, queued: usize) -> HeavyWork {
    HeavyWork::new(HeavyWorkLimits { running, queued })
}

async fn wait_until(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "condition was not reached");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A job that reports its start and then holds its slot until released or cancelled.
fn held_job(
    started: oneshot::Sender<()>,
    release: mpsc::Receiver<()>,
) -> impl FnOnce() -> Result<&'static str, Cancelled> + Send + 'static {
    move || {
        let _ = started.send(());
        loop {
            checkpoint()?;
            if release.recv_timeout(Duration::from_millis(5)).is_ok() {
                return Ok("finished");
            }
        }
    }
}

#[test]
fn checkpoints_pass_outside_a_scope_and_fail_inside_a_cancelled_one() {
    assert!(!is_cancelled());
    assert_eq!(checkpoint(), Ok(()));

    let outer = CancellationToken::new();
    let inner = CancellationToken::new();
    inner.cancel();
    scope(&outer, || {
        assert_eq!(checkpoint(), Ok(()));
        scope(&inner, || assert_eq!(checkpoint(), Err(Cancelled)));
        assert_eq!(checkpoint(), Ok(()), "the outer job is current again");
    });
    assert_eq!(checkpoint(), Ok(()), "the scope is left");
}

#[tokio::test]
async fn jobs_run_on_blocking_threads_and_return_their_value() {
    let heavy = pool(2, 2);
    let runtime_thread = std::thread::current().id();

    let value = heavy
        .run(move || (std::thread::current().id() != runtime_thread, 7))
        .await
        .unwrap();

    assert_eq!(value, (true, 7));
    assert_eq!(heavy.active_jobs(), 0);
}

#[tokio::test]
async fn a_panicking_job_fails_without_leaking_its_slots() {
    let heavy = pool(1, 0);

    let error = heavy.run(|| -> u8 { panic!("expected test panic") }).await;

    assert_eq!(error, Err(HeavyWorkError::Failed));
    assert_eq!(heavy.run(|| 1).await, Ok(1));
    assert_eq!(heavy.active_jobs(), 0);
}

#[tokio::test]
async fn running_and_queued_jobs_are_bounded_and_overload_is_rejected_at_once() {
    let heavy = pool(1, 1);
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let first = tokio::spawn({
        let heavy = heavy.clone();
        async move { heavy.run(held_job(started_tx, release_rx)).await }
    });
    started_rx.await.unwrap();

    let (queued_started_tx, queued_started_rx) = oneshot::channel();
    let (queued_release_tx, queued_release_rx) = mpsc::channel();
    let queued = tokio::spawn({
        let heavy = heavy.clone();
        async move {
            heavy
                .run(held_job(queued_started_tx, queued_release_rx))
                .await
        }
    });
    wait_until(|| heavy.active_jobs() == 2).await;

    let rejected = heavy
        .run(|| unreachable!("an overloaded job must not start"))
        .await;
    assert_eq!(
        rejected,
        Err(HeavyWorkError::Overloaded(HeavyWorkLimits {
            running: 1,
            queued: 1
        }))
    );
    assert_eq!(heavy.active_jobs(), 2, "a rejected job takes no slot");
    assert!(
        rejected.unwrap_err().to_string().contains("MAX_HEAVY_JOBS"),
        "the error names the limits"
    );

    release_tx.send(()).unwrap();
    assert_eq!(first.await.unwrap(), Ok(Ok("finished")));
    queued_started_rx.await.unwrap();
    queued_release_tx.send(()).unwrap();
    assert_eq!(queued.await.unwrap(), Ok(Ok("finished")));
    wait_until(|| heavy.active_jobs() == 0).await;
}

#[tokio::test]
async fn dropping_the_request_cancels_the_job_and_keeps_its_slot_until_it_stops() {
    let heavy = pool(1, 0);
    let (started_tx, started_rx) = oneshot::channel();
    let (observed_tx, observed_rx) = oneshot::channel();
    let (finish_tx, finish_rx) = mpsc::channel::<()>();
    let request = tokio::spawn({
        let heavy = heavy.clone();
        async move {
            heavy
                .run(move || {
                    let _ = started_tx.send(());
                    while checkpoint().is_ok() {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    let _ = observed_tx.send(());
                    // Stay busy after the checkpoint, like a long call that cannot be interrupted.
                    let _ = finish_rx.recv();
                })
                .await
        }
    });
    started_rx.await.unwrap();

    request.abort();
    observed_rx
        .await
        .expect("the job must observe the cancellation");
    assert_eq!(
        heavy.active_jobs(),
        1,
        "a stopping job still holds its slot"
    );
    assert_eq!(
        heavy.run(|| ()).await,
        Err(HeavyWorkError::Overloaded(heavy.limits()))
    );

    finish_tx.send(()).unwrap();
    wait_until(|| heavy.active_jobs() == 0).await;
    assert_eq!(heavy.run(|| 5).await, Ok(5));
}

#[tokio::test]
async fn shutdown_stops_running_jobs_drops_queued_ones_and_discards_results() {
    let heavy = pool(1, 1);
    let (started_tx, started_rx) = oneshot::channel();
    let (_release_tx, release_rx) = mpsc::channel();
    let running = tokio::spawn({
        let heavy = heavy.clone();
        async move { heavy.run(held_job(started_tx, release_rx)).await }
    });
    started_rx.await.unwrap();
    let queued = tokio::spawn({
        let heavy = heavy.clone();
        async move { heavy.run(|| "must not run").await }
    });
    wait_until(|| heavy.active_jobs() == 2).await;

    heavy.shutdown();

    assert_eq!(running.await.unwrap(), Err(HeavyWorkError::Cancelled));
    assert_eq!(queued.await.unwrap(), Err(HeavyWorkError::Cancelled));
    assert_eq!(heavy.run(|| "late").await, Err(HeavyWorkError::Cancelled));
    wait_until(|| heavy.active_jobs() == 0).await;
}

#[tokio::test]
async fn a_result_finished_after_shutdown_is_not_delivered() {
    let heavy = pool(1, 0);
    let (started_tx, started_rx) = oneshot::channel();
    let (finish_tx, finish_rx) = mpsc::channel::<()>();
    let request = tokio::spawn({
        let heavy = heavy.clone();
        async move {
            heavy
                .run(move || {
                    let _ = started_tx.send(());
                    let _ = finish_rx.recv();
                    "finished anyway"
                })
                .await
        }
    });
    started_rx.await.unwrap();

    heavy.shutdown();
    finish_tx.send(()).unwrap();

    assert_eq!(request.await.unwrap(), Err(HeavyWorkError::Cancelled));
}
