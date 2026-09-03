//! Process-local selection among transport-hidden hoist endpoints.

use std::{collections::HashMap, sync::Arc};

use bevy::ecs::resource::Resource;
use weld_hoist_core::HoistEndpoint;

/// Process-local identity of one registered hoist endpoint.
///
/// This is orchestration state, not a device, connection, transport,
/// presentation-target, or client-source identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HoistEndpointId(u64);

impl HoistEndpointId {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Endpoints available to hoist policy in this Weld process.
///
/// IDs are never reused. Entries must remain registered while a
/// [`crate::HoistSession`] can reference them; connection loss is instead
/// reported by [`HoistEndpoint::is_available`].
#[derive(Resource)]
pub struct HoistEndpointRegistry {
    endpoints: HashMap<HoistEndpointId, Arc<dyn HoistEndpoint>>,
    default: Option<HoistEndpointId>,
    next_id: Option<u64>,
}

impl Default for HoistEndpointRegistry {
    fn default() -> Self {
        Self {
            endpoints: HashMap::new(),
            default: None,
            next_id: Some(1),
        }
    }
}

impl HoistEndpointRegistry {
    pub fn with_default(endpoint: impl HoistEndpoint + 'static) -> Self {
        let id = HoistEndpointId(1);
        Self {
            endpoints: HashMap::from([(id, Arc::new(endpoint) as Arc<dyn HoistEndpoint>)]),
            default: Some(id),
            next_id: Some(2),
        }
    }

    /// Registers an endpoint under a fresh process-local identity.
    ///
    /// Returns `None` after the identity space is exhausted. Exhaustion is
    /// permanent so an identity still retained by a session can never alias a
    /// new endpoint.
    pub fn register(&mut self, endpoint: impl HoistEndpoint + 'static) -> Option<HoistEndpointId> {
        let raw = self.next_id?;
        self.next_id = raw.checked_add(1);
        let id = HoistEndpointId(raw);
        self.endpoints.insert(id, Arc::new(endpoint));
        Some(id)
    }

    /// Selects the endpoint used when a new family has no existing route.
    pub fn set_default(&mut self, id: HoistEndpointId) -> bool {
        if !self.endpoints.contains_key(&id) {
            return false;
        }
        self.default = Some(id);
        true
    }

    pub const fn default_id(&self) -> Option<HoistEndpointId> {
        self.default
    }

    pub fn endpoint(&self, id: HoistEndpointId) -> Option<&dyn HoistEndpoint> {
        self.endpoints.get(&id).map(Arc::as_ref)
    }
}

#[cfg(test)]
mod tests {
    use weld_client::{ClientAdapterCommandEnvelope, ClientSourceId, ClientSurfaceId};
    use weld_hoist_core::{HoistEndpointCommand, HoistSessionId};

    use super::*;

    struct TestEndpoint;

    impl HoistEndpoint for TestEndpoint {
        fn destination(&self, source: ClientSurfaceId) -> ClientSurfaceId {
            source
        }

        fn map(
            &self,
            session: HoistSessionId,
            source: ClientSurfaceId,
        ) -> ClientAdapterCommandEnvelope {
            ClientAdapterCommandEnvelope::new(
                ClientSourceId::new(0),
                HoistEndpointCommand::Map { session, source },
            )
        }

        fn unmap(&self, source: ClientSurfaceId) -> ClientAdapterCommandEnvelope {
            ClientAdapterCommandEnvelope::new(
                ClientSourceId::new(0),
                HoistEndpointCommand::Unmap { source },
            )
        }
    }

    #[test]
    fn registration_is_monotonic_and_default_selection_requires_a_known_endpoint() {
        let mut endpoints = HoistEndpointRegistry::with_default(TestEndpoint);
        let first = endpoints.default_id().expect("default endpoint");
        let second = endpoints.register(TestEndpoint).expect("second endpoint");

        assert_ne!(first, second);
        assert_eq!(first.raw(), 1);
        assert_eq!(second.raw(), 2);
        assert!(endpoints.set_default(second));
        assert_eq!(endpoints.default_id(), Some(second));
        assert!(!endpoints.set_default(HoistEndpointId(17)));
        assert_eq!(endpoints.default_id(), Some(second));
    }

    #[test]
    fn exhausted_endpoint_identity_space_never_wraps() {
        let mut endpoints = HoistEndpointRegistry {
            next_id: Some(u64::MAX),
            ..Default::default()
        };

        let final_id = endpoints.register(TestEndpoint).expect("final endpoint ID");

        assert_eq!(final_id.raw(), u64::MAX);
        assert!(endpoints.register(TestEndpoint).is_none());
        assert!(endpoints.register(TestEndpoint).is_none());
    }
}
