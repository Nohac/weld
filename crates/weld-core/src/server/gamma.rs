//! Per-output gamma ownership and validation. Physical hosts explicitly enable
//! the global; ordinary nested/headless servers expose no gamma-control protocol.
use super::ServerState;
use crate::{
    OutputId,
    gamma::{GammaRamp, MAX_GAMMA_ENTRIES},
};
use anyhow::Result;
use smithay::{
    output::Output,
    reexports::{
        wayland_protocols_wlr::gamma_control::v1::server::{
            zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
            zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
        },
        wayland_server::{
            Client, DataInit, DisplayHandle, New, Resource,
            backend::{ClientId, GlobalId},
        },
    },
    wayland::{Dispatch2, GlobalDispatch2},
};
use std::{collections::HashMap, fs::File};

#[derive(Default)]
pub(super) struct GammaControl {
    global: Option<GlobalId>,
    outputs: HashMap<OutputId, OutputGamma>,
}
struct OutputGamma {
    size: u32,
    owner: Option<ZwlrGammaControlV1>,
    desired: Option<GammaRamp>,
    dirty: bool,
}
struct ManagerData;
struct ControlData {
    output: Option<OutputId>,
}

impl ServerState {
    /// Called only by the physical host (and explicit protocol fixtures). The
    /// advertised size comes from the output backend, never from a UI setting.
    pub(crate) fn enable_gamma_control(
        &mut self,
        outputs: impl IntoIterator<Item = (OutputId, u32)>,
    ) {
        for (id, size) in outputs {
            if self.outputs.contains_key(&id) && (1..=MAX_GAMMA_ENTRIES).contains(&size) {
                self.gamma.outputs.entry(id).or_insert(OutputGamma {
                    size,
                    owner: None,
                    desired: None,
                    dirty: false,
                });
            }
        }
        if self.gamma.global.is_none() {
            self.gamma.global = Some(
                self.display_handle
                    .create_global::<Self, ZwlrGammaControlManagerV1, _>(1, ManagerData),
            );
        }
    }

    /// Retain the requested tables over VT/suspend transitions and reapply after
    /// the backend reacquires its device. An unowned output restores its baseline.
    pub(crate) fn reapply_gamma(&mut self) {
        for output in self.gamma.outputs.values_mut() {
            output.dirty = true;
        }
    }

    /// An unavailable output invalidates its controller; a replacement output
    /// must be advertised afresh by its backend. Stale control destruction cannot
    /// reset a later controller's tables.
    pub(crate) fn remove_gamma_output(&mut self, id: OutputId) {
        if let Some(output) = self.gamma.outputs.remove(&id)
            && let Some(owner) = output.owner
        {
            owner.failed();
        }
    }

    /// The physical host calls this only with an active device and no pending
    /// flip on this output. Updates coalesce without requesting a Bevy redraw.
    /// A backend error retires the capability and fails its current controller.
    pub(crate) fn service_gamma(
        &mut self,
        id: OutputId,
        apply: impl FnOnce(Option<&GammaRamp>) -> Result<()>,
    ) -> Result<()> {
        let Some(output) = self
            .gamma
            .outputs
            .get_mut(&id)
            .filter(|output| output.dirty)
        else {
            return Ok(());
        };
        output.dirty = false;
        if let Err(error) = apply(output.desired.as_ref()) {
            self.remove_gamma_output(id);
            return Err(error);
        }
        Ok(())
    }
}

impl GammaControl {
    fn release(&mut self, id: OutputId, resource: &ZwlrGammaControlV1) {
        if let Some(output) = self.outputs.get_mut(&id)
            && output.owner.as_ref() == Some(resource)
        {
            output.owner = None;
            output.desired = None;
            output.dirty = true;
        }
    }
}

impl GlobalDispatch2<ZwlrGammaControlManagerV1, ServerState> for ManagerData {
    fn bind(
        &self,
        _: &mut ServerState,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        init: &mut DataInit<'_, ServerState>,
    ) {
        init.init(resource, ManagerData);
    }
}
impl Dispatch2<ZwlrGammaControlManagerV1, ServerState> for ManagerData {
    fn request(
        &self,
        state: &mut ServerState,
        _: &Client,
        _: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _: &DisplayHandle,
        init: &mut DataInit<'_, ServerState>,
    ) {
        if let zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output } = request {
            let output_id = Output::from_resource(&output).and_then(|output| {
                state
                    .outputs
                    .iter()
                    .find_map(|(id, candidate)| (candidate.native == output).then_some(*id))
            });
            let available = output_id.and_then(|id| {
                state
                    .gamma
                    .outputs
                    .get(&id)
                    .filter(|output| output.owner.is_none())
                    .map(|_| id)
            });
            let control = init.init(id, ControlData { output: available });
            if let Some(output) = available.and_then(|id| state.gamma.outputs.get_mut(&id)) {
                control.gamma_size(output.size);
                output.owner = Some(control);
            } else {
                control.failed();
            }
        }
    }
}
impl Dispatch2<ZwlrGammaControlV1, ServerState> for ControlData {
    fn request(
        &self,
        state: &mut ServerState,
        _: &Client,
        resource: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        _: &DisplayHandle,
        _: &mut DataInit<'_, ServerState>,
    ) {
        let Some(id) = self.output else {
            return;
        };
        match request {
            zwlr_gamma_control_v1::Request::SetGamma { fd } => {
                let Some(output) = state
                    .gamma
                    .outputs
                    .get_mut(&id)
                    .filter(|output| output.owner.as_ref() == Some(resource))
                else {
                    return;
                };
                match GammaRamp::read(&File::from(fd), output.size) {
                    Ok(ramp) => {
                        if output.desired.as_ref() != Some(&ramp) {
                            output.desired = Some(ramp);
                            output.dirty = true;
                        }
                    }
                    Err(error) => {
                        resource.post_error(
                            zwlr_gamma_control_v1::Error::InvalidGamma,
                            format!("invalid gamma table: {error}"),
                        );
                        state.gamma.release(id, resource);
                    }
                }
            }
            zwlr_gamma_control_v1::Request::Destroy => state.gamma.release(id, resource),
            _ => {}
        }
    }
    fn destroyed(&self, state: &mut ServerState, _: ClientId, resource: &ZwlrGammaControlV1) {
        if let Some(id) = self.output {
            state.gamma.release(id, resource);
        }
    }
}
