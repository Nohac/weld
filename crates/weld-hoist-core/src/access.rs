//! Session revocation at the shared source-relay and policy-endpoint boundaries.

use weld_access::AccessSession;
use weld_client::{ClientAdapterCommandEnvelope, ClientAdapterRegistration, ClientSurfaceId};

use crate::{HoistEndpoint, HoistSessionId};

pub(crate) struct SourceAccess(pub AccessSession);

/// Installs the permit before source observation or automatic admission begins.
pub fn authorize_registration(
    registration: &mut ClientAdapterRegistration,
    session: AccessSession,
) {
    registration.configure(SourceAccess(session));
}

/// Couples policy availability and native enforcement to one session permit.
pub fn authorize_source(
    mut registration: ClientAdapterRegistration,
    endpoint: impl HoistEndpoint,
    session: AccessSession,
) -> (ClientAdapterRegistration, impl HoistEndpoint) {
    authorize_registration(&mut registration, session.clone());
    (registration, authorize_endpoint(endpoint, session))
}

/// Applies the shared session-availability gate to a policy endpoint.
pub fn authorize_endpoint<E: HoistEndpoint>(
    endpoint: E,
    session: AccessSession,
) -> AuthorizedEndpoint<E> {
    AuthorizedEndpoint { endpoint, session }
}

#[derive(Clone)]
pub struct AuthorizedEndpoint<E> {
    endpoint: E,
    session: AccessSession,
}

impl<E: HoistEndpoint> HoistEndpoint for AuthorizedEndpoint<E> {
    fn is_available(&self) -> bool {
        self.session.is_authorized() && self.endpoint.is_available()
    }
    fn has_local_receiver(&self) -> bool {
        self.endpoint.has_local_receiver()
    }
    fn destination(&self, source: ClientSurfaceId) -> ClientSurfaceId {
        self.endpoint.destination(source)
    }
    fn map(
        &self,
        session: HoistSessionId,
        source: ClientSurfaceId,
    ) -> ClientAdapterCommandEnvelope {
        self.endpoint.map(session, source)
    }
    fn unmap(&self, source: ClientSurfaceId) -> ClientAdapterCommandEnvelope {
        self.endpoint.unmap(source)
    }
}
