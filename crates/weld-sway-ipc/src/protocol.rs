//! Sway message IDs and JSON shapes used by the binding-mode service.

use anyhow::Result;
use serde::{Deserialize, Serialize};

const RUN_COMMAND: u32 = 0;
const SUBSCRIBE: u32 = 2;
const GET_BINDING_MODES: u32 = 8;
const GET_BINDING_STATE: u32 = 12;
const MODE_EVENT: u32 = (1 << 31) | 2;

#[derive(Debug, Deserialize, PartialEq)]
#[serde(from = "String")]
pub(crate) enum EventKind {
    Mode,
    Unsupported,
}

impl From<String> for EventKind {
    fn from(name: String) -> Self {
        match name.as_str() {
            "mode" => Self::Mode,
            _ => Self::Unsupported,
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum Request {
    RunCommand,
    Subscribe(Vec<EventKind>),
    GetBindingModes,
    GetBindingState,
    Unsupported(u32),
}

impl Request {
    pub(crate) fn decode(kind: u32, payload: &[u8]) -> Result<Self> {
        Ok(match kind {
            RUN_COMMAND => Self::RunCommand,
            SUBSCRIBE => Self::Subscribe(serde_json::from_slice(payload)?),
            GET_BINDING_MODES => Self::GetBindingModes,
            GET_BINDING_STATE => Self::GetBindingState,
            other => Self::Unsupported(other),
        })
    }
}

#[derive(Serialize)]
pub(crate) struct BindingState<'a> {
    pub name: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ModeEvent {
    pub change: String,
    pub pango_markup: bool,
}

#[derive(Serialize)]
struct Failure {
    success: bool,
    error: &'static str,
}

pub(crate) enum Response<'a> {
    Subscription { success: bool },
    BindingModes(&'a [String]),
    BindingState(BindingState<'a>),
    Mode(&'a ModeEvent),
    CommandRejected,
    Unsupported(u32),
}

impl Response<'_> {
    pub(crate) fn encode(self) -> Result<(u32, Vec<u8>)> {
        Ok(match self {
            Self::Subscription { success } => {
                // Waybar's IPC client compares the acknowledgement byte-for-byte.
                let payload: &[u8] = if success {
                    br#"{"success": true}"#
                } else {
                    br#"{"success": false}"#
                };
                (SUBSCRIBE, payload.to_vec())
            }
            Self::BindingModes(names) => (GET_BINDING_MODES, serde_json::to_vec(names)?),
            Self::BindingState(state) => (GET_BINDING_STATE, serde_json::to_vec(&state)?),
            Self::Mode(event) => (MODE_EVENT, serde_json::to_vec(event)?),
            Self::CommandRejected => (
                RUN_COMMAND,
                serde_json::to_vec(&[Failure {
                    success: false,
                    error: "command execution is unavailable on this endpoint",
                }])?,
            ),
            Self::Unsupported(kind) => (
                kind,
                serde_json::to_vec(&Failure {
                    success: false,
                    error: "unsupported IPC request",
                })?,
            ),
        })
    }
}
