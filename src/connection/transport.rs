//! Adapts optional transport diagnostics to the shared connection machinery.

#[cfg(feature = "protocol_udp")]
use crate::protocols::udp::UdpConnectionHandle;

/// Shared diagnostics survive connection closure alongside retained ECS handles.
#[derive(Clone, Default)]
pub struct ConnectionDiagnostics {
    #[cfg(feature = "protocol_udp")]
    udp: Option<UdpConnectionHandle>,
}

impl ConnectionDiagnostics {
    #[cfg(any(feature = "client", feature = "server"))]
    pub fn from_stream(stream: &impl crate::protocols::protocol::NetworkStream) -> Self {
        #[cfg(feature = "protocol_udp")]
        {
            Self::from_udp(stream.udp())
        }
        #[cfg(not(feature = "protocol_udp"))]
        {
            let _ = stream;
            Self::default()
        }
    }

    #[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
    pub const fn from_udp(udp: Option<UdpConnectionHandle>) -> Self {
        Self { udp }
    }

    #[cfg(feature = "protocol_udp")]
    pub fn udp(&self) -> Option<UdpConnectionHandle> {
        self.udp.clone()
    }

    #[cfg(any(feature = "client", feature = "server"))]
    #[cfg_attr(
        not(feature = "protocol_udp"),
        expect(
            clippy::unused_self,
            clippy::missing_const_for_fn,
            reason = "The same interface updates shared counters when UDP diagnostics are enabled"
        )
    )]
    pub fn record_drop(&self, reason: QueueDropReason) {
        #[cfg(feature = "protocol_udp")]
        if let Some(udp) = &self.udp {
            use crate::protocols::udp::UdpDropReason;

            let reason = match reason {
                QueueDropReason::OutgoingQueueFull => UdpDropReason::OutgoingQueueFull,
                QueueDropReason::OutgoingQueueEvicted => UdpDropReason::OutgoingQueueEvicted,
                QueueDropReason::ReceiveQueueFull => UdpDropReason::ReceiveQueueFull,
                QueueDropReason::ReceiveQueueEvicted => UdpDropReason::ReceiveQueueEvicted,
                QueueDropReason::ClosedBeforeDelivery => UdpDropReason::ClosedBeforeDelivery,
            };
            udp.count_drop(reason);
        }
        #[cfg(not(feature = "protocol_udp"))]
        let _ = reason;
    }
}

/// Losses in the outgoing or decoded packet queues, independent of wire format.
#[cfg(any(feature = "client", feature = "server"))]
pub enum QueueDropReason {
    OutgoingQueueFull,
    OutgoingQueueEvicted,
    ReceiveQueueFull,
    ReceiveQueueEvicted,
    ClosedBeforeDelivery,
}

/// Counts a dequeued packet if its send is interrupted before completion.
#[cfg(any(feature = "client", feature = "server"))]
pub struct PendingPacket(Option<ConnectionDiagnostics>);

#[cfg(any(feature = "client", feature = "server"))]
impl PendingPacket {
    pub const fn new(diagnostics: ConnectionDiagnostics) -> Self {
        Self(Some(diagnostics))
    }

    pub fn finish(&mut self, result: &std::io::Result<()>) {
        // Other failures are handled by the transport or serialization layer.
        // Completion does not imply a successful socket send or remote delivery.
        if !matches!(result, Err(err) if err.kind() == std::io::ErrorKind::ConnectionAborted) {
            self.0 = None;
        }
    }
}

#[cfg(any(feature = "client", feature = "server"))]
impl Drop for PendingPacket {
    fn drop(&mut self) {
        if let Some(diagnostics) = self.0.take() {
            diagnostics.record_drop(QueueDropReason::ClosedBeforeDelivery);
        }
    }
}

/// Installs app-local timeout updates for transports that support them.
#[cfg(any(feature = "client", feature = "server"))]
#[cfg_attr(
    not(feature = "protocol_udp"),
    expect(
        clippy::needless_pass_by_ref_mut,
        reason = "The enabled transport installs resources and systems in this app"
    )
)]
pub fn install_idle_timeout(
    app: &mut bevy::prelude::App,
) -> tokio::sync::watch::Receiver<std::time::Duration> {
    #[cfg(feature = "protocol_udp")]
    {
        crate::protocols::udp::IdleTimeoutSettings::install(app)
    }
    #[cfg(not(feature = "protocol_udp"))]
    {
        let _ = app;
        tokio::sync::watch::channel(std::time::Duration::MAX).1
    }
}
