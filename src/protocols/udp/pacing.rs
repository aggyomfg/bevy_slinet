use std::{io, num::NonZeroU64, time::Duration};
use tokio::{sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;

/// Spaces datagram sends by their full wire length; unused time never accumulates credit.
pub(super) struct DataPacer {
    rate: watch::Receiver<Option<NonZeroU64>>,
    previous: Option<(Instant, usize)>,
}

impl DataPacer {
    pub(super) const fn new(rate: watch::Receiver<Option<NonZeroU64>>) -> Self {
        Self {
            rate,
            previous: None,
        }
    }

    pub(super) async fn wait(&mut self, closed: &CancellationToken) -> io::Result<()> {
        loop {
            let rate = *self.rate.borrow_and_update();
            let Some((sent_at, bytes)) = self.previous else {
                return Ok(());
            };
            let Some(rate) = rate else {
                return Ok(());
            };
            let deadline = sent_at + spacing(bytes, rate);
            if deadline <= Instant::now() {
                return Ok(());
            }
            tokio::select! {
                biased;
                () = closed.cancelled() => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "UDP peer closed")),
                changed = self.rate.changed() => {
                    if changed.is_err() { return Ok(()); }
                }
                () = tokio::time::sleep_until(deadline) => return Ok(()),
            }
        }
    }

    pub(super) fn sent(&mut self, bytes: usize) {
        self.previous = Some((Instant::now(), bytes));
    }
}

fn spacing(bytes: usize, rate: NonZeroU64) -> Duration {
    let nanos = (bytes as u128 * 1_000_000_000u128).div_ceil(u128::from(rate.get()));
    Duration::new(
        u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX),
        u32::try_from(nanos % 1_000_000_000).unwrap_or(u32::MAX),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spacing_counts_all_wire_bytes_and_rounds_up() {
        assert_eq!(
            spacing(37, NonZeroU64::new(74).unwrap()),
            Duration::from_millis(500)
        );
        assert_eq!(
            spacing(1, NonZeroU64::new(3).unwrap()).as_nanos(),
            333_333_334
        );
    }
}
