//! Optional endpoint-specific overrides of app-wide network settings.
use super::{MaxPacketSize, NetworkQueueSettings, ReceiveLimits};
use bevy::prelude::{Res, Resource};
use std::marker::PhantomData;

/// Settings for one endpoint role and config type.
/// Use `client::ClientSettings<C>` or `server::ServerSettings<C>` as a resource.
/// Missing overrides inherit the app-wide resources, then library defaults.
#[derive(Resource)]
pub struct NetworkSettings<Endpoint: Send + Sync + 'static> {
    /// Queue settings. Capacities and overflow policies are captured at startup;
    /// `events_per_frame` is read each frame.
    pub queues: Option<NetworkQueueSettings>,
    /// Maximum incoming serialized payload bytes, applied at startup and Update.
    /// `None` inherits `MaxPacketSize`; `Some(usize::MAX)` explicitly removes the limit.
    pub max_packet_size: Option<usize>,
    marker: PhantomData<fn() -> Endpoint>,
}
impl<E: Send + Sync + 'static> Default for NetworkSettings<E> {
    fn default() -> Self {
        Self {
            queues: None,
            max_packet_size: None,
            marker: PhantomData,
        }
    }
}
impl<E: Send + Sync + 'static> NetworkSettings<E> {
    /// Overrides queue settings for this endpoint.
    #[must_use]
    pub const fn with_queues(mut self, queues: NetworkQueueSettings) -> Self {
        self.queues = Some(queues);
        self
    }
    /// Overrides the incoming payload limit for this endpoint.
    #[must_use]
    pub const fn with_max_packet_size(mut self, bytes: usize) -> Self {
        self.max_packet_size = Some(bytes);
        self
    }
    pub(crate) fn resolve_queues(
        settings: Option<&Self>,
        global: Option<&NetworkQueueSettings>,
    ) -> NetworkQueueSettings {
        settings
            .and_then(|s| s.queues)
            .or_else(|| global.copied())
            .unwrap_or_default()
    }
    pub(crate) fn sync_limits(
        settings: Option<Res<Self>>,
        global: Option<Res<MaxPacketSize>>,
        limits: Res<EndpointReceiveLimits<E>>,
    ) {
        limits.limits.set_max_packet_size(
            settings
                .and_then(|s| s.max_packet_size)
                .or_else(|| global.map(|g| g.0))
                .unwrap_or(usize::MAX),
        );
    }
    pub(crate) fn warning_system(settings: Option<Res<Self>>, global: Option<Res<MaxPacketSize>>) {
        if settings.and_then(|s| s.max_packet_size).is_none() && global.is_none() {
            bevy::log::warn!(
                "No incoming packet size limit configured for {}",
                std::any::type_name::<E>()
            );
        }
    }
}
#[derive(Resource)]
pub struct EndpointReceiveLimits<E: Send + Sync + 'static> {
    pub(crate) limits: ReceiveLimits,
    marker: PhantomData<fn() -> E>,
}
impl<E: Send + Sync + 'static> Default for EndpointReceiveLimits<E> {
    fn default() -> Self {
        Self {
            limits: ReceiveLimits::default(),
            marker: PhantomData,
        }
    }
}
