/// Bounds real socket scenarios even while Tokio's clock is paused.
pub async fn wall_timeout<F: std::future::Future>(future: F) -> F::Output {
    let (finished, completion) = std::sync::mpsc::channel::<()>();
    let (expired, deadline) = tokio::sync::oneshot::channel();
    let _watchdog = std::thread::spawn(move || {
        if matches!(
            completion.recv_timeout(std::time::Duration::from_secs(5)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ) {
            let _ = expired.send(());
        }
    });
    let result = tokio::select! {
        result = future => result,
        _ = deadline => panic!("UDP socket test exceeded its five-second wall-clock budget"),
    };
    drop(finished);
    result
}
