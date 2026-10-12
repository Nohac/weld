//! Local administrative access-session operations.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use weld_access::{AccessAuthority, AccessSessionId};

#[derive(Serialize, Deserialize)]
pub enum AccessRequest {
    Status,
    RevokeAll,
    Revoke(AccessSessionId),
    Allow,
}

impl AccessRequest {
    pub(crate) fn execute(&self, access: &AccessAuthority) -> Result<crate::Response> {
        Ok(match self {
            Self::Status => crate::Response::AccessStatus(access.status()?),
            Self::RevokeAll => {
                access.revoke_all()?;
                crate::Response::Ok
            }
            Self::Revoke(id) => {
                access.revoke(*id)?;
                crate::Response::Ok
            }
            Self::Allow => {
                access.allow_new_sessions()?;
                crate::Response::Ok
            }
        })
    }
}
