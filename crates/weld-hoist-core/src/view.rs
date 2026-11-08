//! Hoist scope identities over the shared window viewport assembly.

pub type SourceViews = weld_client::SourceViews<crate::HoistSessionId>;
pub type ViewTarget = weld_client::ViewTarget<crate::HoistSessionId>;

#[cfg(test)]
mod tests;
