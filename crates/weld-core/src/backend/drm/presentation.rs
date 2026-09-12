//! Shared Smithay output-manager ownership and batched physical presentation.

use std::{collections::HashMap, path::Path, time::Duration};

use anyhow::{Context, Result};
use smithay::{
    backend::drm::{
        DrmError,
        compositor::{FrameError, FrameFlags, PrimaryPlaneElement, RenderFrameError},
        output::DrmOutputRenderElements,
    },
    reexports::drm::control::crtc,
};
use tracing::warn;

use crate::{
    OutputId, OutputScale,
    cursor::{CursorConfiguration, CursorImage},
    host::{CompositionFrame, CompositionHost},
    input::InputPosition,
    server::ServerState,
};

use crate::runtime::callbacks::{CallbackLedger, complete_callback_batches, stage_callback_batch};

use super::{
    cursor::{CursorResources, CursorState},
    device::{OutputManager, PhysicalOutput, SubmittedFrame},
    output::SelectedOutput,
    renderer::{BatchFinish, CompositionElement, DrmRenderState, DrmRenderer, OutputElement},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrameAdmission {
    Idle,
    Queued {
        presentation_id: Option<u64>,
        deferred_present: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RetiredFrame {
    pub(super) presentation_id: Option<u64>,
    pub(super) deferred_present: bool,
}

impl FrameAdmission {
    const fn is_idle(self) -> bool {
        matches!(self, Self::Idle)
    }

    fn queue(&mut self, presentation_id: Option<u64>) {
        *self = Self::Queued {
            presentation_id,
            deferred_present: false,
        };
    }

    fn defer_present(&mut self) {
        if let Self::Queued {
            deferred_present, ..
        } = self
        {
            *deferred_present = true;
        }
    }

    fn retire(&mut self) -> Option<RetiredFrame> {
        let Self::Queued {
            presentation_id,
            deferred_present,
        } = std::mem::replace(self, Self::Idle)
        else {
            return None;
        };
        Some(RetiredFrame {
            presentation_id,
            deferred_present,
        })
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct PhysicalRenderOutcome {
    pub(super) queued: Vec<OutputId>,
    pub(super) empty: Vec<OutputId>,
    pub(super) busy: Vec<OutputId>,
    pub(super) retry: Vec<OutputId>,
    pub(super) inactive: bool,
}

struct PhysicalOutputState {
    id: OutputId,
    crtc: crtc::Handle,
    output: PhysicalOutput,
    composition: CompositionElement,
    cursor: CursorState,
    admission: FrameAdmission,
    logical_origin: crate::surface::LogicalPoint,
    scale: OutputScale,
    available: bool,
}

pub(super) struct PhysicalDesktop {
    manager: OutputManager,
    render_state: DrmRenderState,
    outputs: Vec<PhysicalOutputState>,
}

impl PhysicalDesktop {
    pub(super) fn new(
        mut manager: OutputManager,
        mut render_state: DrmRenderState,
        selected_outputs: &[SelectedOutput],
        server: &ServerState,
    ) -> Result<Self> {
        let mut outputs = Vec::with_capacity(selected_outputs.len());
        let cursor_resources = CursorResources::default();
        for selected in selected_outputs {
            let native_output = server
                .native_output(selected.id)
                .context("Wayland server did not install a selected DRM output")?;
            let output = {
                let mut renderer = render_state.renderer(selected.id);
                let render_elements: DrmOutputRenderElements<
                    DrmRenderer<'_>,
                    OutputElement<DrmRenderer<'_>>,
                > = DrmOutputRenderElements::default();
                manager
                    .lock()
                    .initialize_output(
                        selected.crtc,
                        selected.mode,
                        &[selected.connector.handle()],
                        &native_output,
                        None,
                        &mut renderer,
                        &render_elements,
                    )
                    .with_context(|| {
                        format!("Smithay failed to initialize DRM output {:?}", selected.id)
                    })?
            };
            outputs.push(PhysicalOutputState {
                id: selected.id,
                crtc: selected.crtc,
                output,
                composition: CompositionElement::new(selected.id, selected.configuration.extent())?,
                cursor: CursorState::new(
                    CursorConfiguration::default(),
                    selected.configuration.scale().value(),
                    cursor_resources.clone(),
                )?,
                admission: FrameAdmission::Idle,
                logical_origin: selected.configuration.position(),
                scale: selected.configuration.scale(),
                available: true,
            });
        }
        if let Some(first) = selected_outputs.first() {
            let mut renderer = render_state.renderer(first.id);
            let render_elements: DrmOutputRenderElements<
                DrmRenderer<'_>,
                OutputElement<DrmRenderer<'_>>,
            > = DrmOutputRenderElements::default();
            manager
                .lock()
                .try_to_restore_modifiers(&mut renderer, &render_elements)
                .context("Smithay failed to restore explicit output modifiers")?;
        }
        Ok(Self {
            manager,
            render_state,
            outputs,
        })
    }

    pub(super) fn request_all_compositions(&mut self) {
        for output in &mut self.outputs {
            output.composition.mark_dirty();
        }
    }

    pub(super) fn unavailable_output_ids(&self) -> impl Iterator<Item = OutputId> + '_ {
        self.outputs
            .iter()
            .filter(|output| !output.available)
            .map(|output| output.id)
    }

    pub(super) fn set_cursor_position(&mut self, position: InputPosition) {
        for output in &mut self.outputs {
            output.cursor.set_position(InputPosition::new(
                position.x - f64::from(output.logical_origin.x),
                position.y - f64::from(output.logical_origin.y),
            ));
        }
    }

    pub(super) fn set_cursor_image(&mut self, image: CursorImage) -> Result<()> {
        for output in &mut self.outputs {
            output.cursor.set_image(image.clone())?;
        }
        Ok(())
    }

    pub(super) fn set_cursor_configuration(
        &mut self,
        configuration: CursorConfiguration,
    ) -> Result<()> {
        for output in &mut self.outputs {
            output.cursor.set_configuration(configuration.clone())?;
        }
        Ok(())
    }

    pub(super) fn update_configuration(
        &mut self,
        output_id: OutputId,
        configuration: crate::OutputConfiguration,
    ) -> Result<()> {
        let output = self
            .output_mut(output_id)
            .context("updated output is not physically available")?;
        output.logical_origin = configuration.position();
        output.scale = configuration.scale();
        output.cursor.set_scale(configuration.scale().value())
    }

    pub(super) fn render(
        &mut self,
        requested_outputs: &[OutputId],
        host: &mut dyn CompositionHost,
        server: &mut ServerState,
        callbacks: &mut CallbackLedger,
        vblank_phases: &HashMap<OutputId, Duration>,
        stage_callbacks: bool,
    ) -> Result<PhysicalRenderOutcome> {
        let mut outcome = PhysicalRenderOutcome::default();
        let mut indices = Vec::with_capacity(requested_outputs.len());
        for output_id in requested_outputs {
            let Some(index) = self
                .outputs
                .iter()
                .position(|output| output.id == *output_id && output.available)
            else {
                continue;
            };
            if !self.outputs[index].admission.is_idle() {
                self.outputs[index].admission.defer_present();
                outcome.busy.push(*output_id);
            } else {
                indices.push(index);
            }
        }
        if indices.is_empty() {
            return Ok(outcome);
        }

        self.render_state.begin_batch()?;
        let prepared = match self.prepare_outputs(&indices) {
            PrepareBatch::Ready(prepared) => prepared,
            PrepareBatch::Inactive => {
                self.render_state.abort_batch();
                outcome.inactive = true;
                return Ok(outcome);
            }
        };
        match self.render_state.finish_batch(host)? {
            BatchFinish::Complete => {}
            BatchFinish::CompositionFailed(error) => {
                warn!(%error, "Bevy DRM composition failed; retaining output damage for retry");
                outcome
                    .retry
                    .extend(indices.iter().map(|index| self.outputs[*index].id));
                return Ok(outcome);
            }
        }

        let candidates = prepared
            .iter()
            .filter(|frame| !frame.empty)
            .map(|frame| self.outputs[frame.index].id)
            .collect::<Vec<_>>();
        let presentation_id = stage_callbacks
            .then(|| stage_callback_batch(callbacks, server, candidates.iter().copied()))
            .flatten();
        for frame in prepared {
            let output = &mut self.outputs[frame.index];
            if frame.empty {
                outcome.empty.push(output.id);
                continue;
            }
            match output
                .output
                .queue_frame(SubmittedFrame { presentation_id })
            {
                Ok(()) => {
                    output.admission.queue(presentation_id);
                    outcome.queued.push(output.id);
                    tracing::trace!(
                        target: "weld_drm_pacing",
                        output = ?output.id,
                        batch_outputs = requested_outputs.len(),
                        hardware_cursor = frame.hardware_cursor,
                        composition_drawn = self.render_state.composition_drawn(output.id),
                        needs_sync = frame.needs_sync,
                        gpu_wait_micros = self.render_state.last_gpu_wait().as_micros(),
                        vblank_phase_micros = vblank_phases.get(&output.id).map(Duration::as_micros),
                        "queued physical frame"
                    );
                }
                Err(FrameError::EmptyFrame) => {
                    outcome.empty.push(output.id);
                    let completed = callbacks.remove_output(output.id);
                    complete_callback_batches(server, completed);
                }
                Err(FrameError::DrmError(DrmError::DeviceInactive)) => {
                    outcome.inactive = true;
                    let completed = callbacks.remove_output(output.id);
                    complete_callback_batches(server, completed);
                }
                Err(error) => {
                    output.available = false;
                    let completed = callbacks.remove_output(output.id);
                    complete_callback_batches(server, completed);
                    warn!(output = ?output.id, ?error, "disabled a failed physical output");
                }
            }
        }
        Ok(outcome)
    }

    fn prepare_outputs(&mut self, indices: &[usize]) -> PrepareBatch {
        let mut prepared = Vec::with_capacity(indices.len());
        for index in indices.iter().copied() {
            let output = &mut self.outputs[index];
            let mut renderer = self.render_state.renderer(output.id);
            let cursor = match output.cursor.render_element(&mut renderer) {
                Ok(cursor) => cursor,
                Err(error) => {
                    output.available = false;
                    warn!(output = ?output.id, %error, "disabled output after cursor import failed");
                    continue;
                }
            };
            let mut elements: Vec<OutputElement<DrmRenderer<'_>>> = Vec::with_capacity(2);
            if let Some(cursor) = cursor {
                elements.push(OutputElement::from(cursor));
            }
            elements.push(OutputElement::from(output.composition.clone()));
            let result = match output.output.render_frame(
                &mut renderer,
                &elements,
                smithay::backend::renderer::Color32F::BLACK,
                FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT,
            ) {
                Ok(result) => result,
                Err(RenderFrameError::PrepareFrame(FrameError::DrmError(
                    DrmError::DeviceInactive,
                ))) => return PrepareBatch::Inactive,
                Err(error) => {
                    output.available = false;
                    warn!(output = ?output.id, ?error, "disabled a failed physical output");
                    continue;
                }
            };
            let needs_sync = result.needs_sync();
            if needs_sync
                && let PrimaryPlaneElement::Swapchain(primary) = &result.primary_element
                && let Err(error) = primary.sync.wait()
            {
                output.available = false;
                warn!(output = ?output.id, %error, "disabled output after render synchronization failed");
                continue;
            }
            prepared.push(PreparedOutput {
                index,
                empty: result.is_empty,
                hardware_cursor: result.cursor_element.is_some(),
                needs_sync,
            });
        }
        PrepareBatch::Ready(prepared)
    }

    pub(super) fn retire(
        &mut self,
        crtc: crtc::Handle,
        server: &mut ServerState,
        callbacks: &mut CallbackLedger,
    ) -> Result<Option<(OutputId, RetiredFrame)>> {
        let Some(output) = self.outputs.iter_mut().find(|output| output.crtc == crtc) else {
            return Ok(None);
        };
        let submitted = output
            .output
            .frame_submitted()
            .with_context(|| format!("Smithay failed to retire output {:?}", output.id))?;
        let Some(submitted) = submitted else {
            return Ok(None);
        };
        let admission = output.admission.retire().unwrap_or(RetiredFrame {
            presentation_id: None,
            deferred_present: false,
        });
        if submitted.presentation_id != admission.presentation_id {
            warn!(
                output = ?output.id,
                queued = ?admission.presentation_id,
                submitted = ?submitted.presentation_id,
                "physical frame callback identity diverged"
            );
        }
        if let Some(id) = submitted.presentation_id.or(admission.presentation_id) {
            let completed = callbacks.retire(id, output.id);
            complete_callback_batches(server, completed);
        }
        Ok(Some((output.id, admission)))
    }

    pub(super) fn pause(&mut self) {
        self.manager.pause();
    }

    pub(super) fn activate(
        &mut self,
        server: &mut ServerState,
        callbacks: &mut CallbackLedger,
    ) -> Result<()> {
        self.manager
            .lock()
            .activate(true)
            .context("failed to reactivate the Smithay DRM output manager")?;
        for output in &mut self.outputs {
            if let Some(frame) = output.admission.retire()
                && let Some(id) = frame.presentation_id
            {
                let completed = callbacks.retire(id, output.id);
                complete_callback_batches(server, completed);
            }
            output.composition.mark_dirty();
        }
        Ok(())
    }

    pub(super) fn capture_owned(&self, frame: &CompositionFrame, path: &Path) -> Result<()> {
        crate::renderer::capture_owned_frame(
            self.render_state.device(),
            self.render_state.queue(),
            frame,
            path,
        )
    }

    fn output_mut(&mut self, id: OutputId) -> Option<&mut PhysicalOutputState> {
        self.outputs.iter_mut().find(|output| output.id == id)
    }
}

struct PreparedOutput {
    index: usize,
    empty: bool,
    hardware_cursor: bool,
    needs_sync: bool,
}

enum PrepareBatch {
    Ready(Vec<PreparedOutput>),
    Inactive,
}
