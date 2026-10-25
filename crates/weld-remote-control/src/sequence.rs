//! Validated keyboard sequences and bounded, deadline-paced execution plans.

mod wire;

use crate::{InputReceipt, Operation};
use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};
use weld_client::{LinuxKeycode, TextInput};

/// Named key transition within a sequence. Held keys must be released in that sequence.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeyState {
    #[default]
    Press,
    Down,
    Up,
}

/// An XKB key name or a Linux evdev keycode.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum InputKey {
    Name(String),
    Code(u32),
}

impl From<&str> for InputKey {
    fn from(name: &str) -> Self {
        Self::Name(name.into())
    }
}

impl From<String> for InputKey {
    fn from(name: String) -> Self {
        Self::Name(name)
    }
}

impl InputKey {
    fn validate(&self, names: &mut Vec<String>) -> Result<()> {
        match self {
            Self::Name(name) => {
                ensure!(
                    !name.is_empty() && name.len() <= 64 && !name.contains('\0'),
                    "invalid key name"
                );
                names.push(name.clone());
            }
            Self::Code(code) => ensure!((1..=767).contains(code), "invalid Linux keycode"),
        }
        Ok(())
    }

    fn resolve(self, codes: &mut impl Iterator<Item = LinuxKeycode>) -> Result<u32> {
        match self {
            Self::Name(_) => Ok(codes.next().context("missing resolved key")?.0),
            Self::Code(code) => Ok(code),
        }
    }
}

/// Text, a named key transition/chord, or an explicit pause.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(from = "wire::Action", into = "wire::Action")]
pub enum InputAction {
    Text(String),
    Keys {
        keys: Vec<InputKey>,
        state: KeyState,
        /// Pause after this action, overriding the global key interval.
        delay_ms: Option<u32>,
        /// Duration between press and release; valid for `press` actions.
        hold_ms: Option<u32>,
    },
    Wait {
        wait_ms: u32,
    },
}

impl std::str::FromStr for InputAction {
    type Err = serde_json::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(value)
    }
}

/// Progress counts native effects acknowledged by the relay. Application processing follows.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct InputSequenceResult {
    pub completed_actions: usize,
    pub typed_characters: usize,
    pub complete: bool,
    pub receipt: Option<InputReceipt>,
    pub error: Option<String>,
    /// A dispatched step may have taken effect beyond the reported completed prefix.
    pub in_flight: bool,
}

impl std::fmt::Display for InputSequenceResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} actions; {} characters; complete={}",
            self.completed_actions, self.typed_characters, self.complete
        )
    }
}

pub(crate) struct Plan {
    actions: Vec<InputAction>,
    pub names: Vec<String>,
    interval: Duration,
    key_interval_ms: u32,
    key_hold_ms: u32,
    pub has_text: bool,
}

pub(crate) enum Step {
    Input(InputStep),
    Wait(Duration),
}

pub(crate) struct InputStep {
    pub operation: Operation,
    pub ends_action: bool,
    pub characters: usize,
    pub delay_after: Duration,
}

impl Plan {
    pub fn new(
        actions: Vec<InputAction>,
        typing_interval_ms: u32,
        key_interval_ms: u32,
        key_hold_ms: u32,
    ) -> Result<Self> {
        ensure!(
            !actions.is_empty() && actions.len() <= 64,
            "input needs 1 to 64 actions"
        );
        ensure!(
            typing_interval_ms <= 1000,
            "typing_interval_ms must be between 0 and 1000"
        );
        ensure!(
            key_interval_ms <= 1000,
            "key_interval_ms must be between 0 and 1000"
        );
        ensure!(
            key_hold_ms <= 1000,
            "key_hold_ms must be between 0 and 1000"
        );
        let mut names = Vec::new();
        let mut bytes = 0;
        let mut duration_ms = 0u64;
        let mut has_text = false;
        for action in &actions {
            let keys = match action {
                InputAction::Text(text) => {
                    TextInput::try_from(text.clone()).map_err(anyhow::Error::msg)?;
                    bytes += text.len();
                    duration_ms += text.chars().count().saturating_sub(1) as u64
                        * u64::from(typing_interval_ms);
                    has_text = true;
                    continue;
                }
                InputAction::Keys {
                    keys,
                    state,
                    delay_ms,
                    hold_ms,
                } => {
                    ensure!(
                        !keys.is_empty() && keys.len() <= 16,
                        "key action needs 1 to 16 keys"
                    );
                    duration_ms += u64::from(delay_ms.unwrap_or(key_interval_ms));
                    if matches!(state, KeyState::Press) {
                        duration_ms += u64::from(hold_ms.unwrap_or(key_hold_ms));
                    } else {
                        ensure!(hold_ms.is_none(), "hold_ms requires a complete keypress");
                    }
                    keys.as_slice()
                }
                InputAction::Wait { wait_ms } => {
                    duration_ms += u64::from(*wait_ms);
                    continue;
                }
            };
            for key in keys {
                key.validate(&mut names)?;
            }
        }
        ensure!(
            bytes <= TextInput::MAX_BYTES,
            "combined text exceeds 4096 UTF-8 bytes"
        );
        ensure!(
            duration_ms <= 25_000,
            "planned input duration exceeds 25 seconds; split the sequence or reduce the delays"
        );
        Ok(Self {
            actions,
            names,
            interval: Duration::from_millis(u64::from(typing_interval_ms)),
            key_interval_ms,
            key_hold_ms,
            has_text,
        })
    }

    pub fn resolve(self, codes: Vec<LinuxKeycode>) -> Result<VecDeque<Step>> {
        ensure!(
            codes.len() == self.names.len(),
            "incomplete keyboard resolution"
        );
        ensure!(
            codes.iter().all(|key| (1..=767).contains(&key.0)),
            "keyboard resolution returned an invalid keycode"
        );
        let mut codes = codes.into_iter();
        let mut steps = VecDeque::new();
        let mut held = HashSet::new();
        for action in self.actions {
            if matches!(action, InputAction::Text(_)) {
                ensure!(held.is_empty(), "release sequence keys before typing text");
            }
            match action {
                InputAction::Text(text) if self.interval.is_zero() => {
                    steps.push_back(Step::Input(InputStep {
                        characters: text.chars().count(),
                        operation: Operation::TypeText { text },
                        ends_action: true,
                        delay_after: Duration::ZERO,
                    }))
                }
                InputAction::Text(text) => {
                    let mut chars = text.chars().peekable();
                    while let Some(character) = chars.next() {
                        let ends_action = chars.peek().is_none();
                        steps.push_back(Step::Input(InputStep {
                            operation: Operation::TypeText {
                                text: character.to_string(),
                            },
                            characters: 1,
                            ends_action,
                            delay_after: if ends_action {
                                Duration::ZERO
                            } else {
                                self.interval
                            },
                        }));
                    }
                }
                InputAction::Keys {
                    keys,
                    state,
                    delay_ms,
                    hold_ms,
                } => {
                    let keycodes = keys
                        .into_iter()
                        .map(|key| key.resolve(&mut codes))
                        .collect::<Result<Vec<_>>>()?;
                    ensure!(
                        keycodes.iter().copied().collect::<HashSet<_>>().len() == keycodes.len(),
                        "key action resolves to duplicate keys"
                    );
                    let delay_ms = delay_ms.unwrap_or(self.key_interval_ms);
                    let operation = match state {
                        KeyState::Press => {
                            ensure!(
                                keycodes.iter().all(|key| !held.contains(key)),
                                "keypress key is already held"
                            );
                            push_press(
                                &mut steps,
                                keycodes,
                                hold_ms.unwrap_or(self.key_hold_ms),
                                delay_ms,
                            );
                            continue;
                        }
                        KeyState::Down => {
                            for key in &keycodes {
                                ensure!(held.insert(*key), "key is already held");
                            }
                            Operation::Keys {
                                keycodes,
                                pressed: true,
                            }
                        }
                        KeyState::Up => {
                            for key in &keycodes {
                                ensure!(held.remove(key), "key up has no matching key down");
                            }
                            Operation::Keys {
                                keycodes,
                                pressed: false,
                            }
                        }
                    };
                    steps.push_back(Step::Input(InputStep {
                        operation,
                        ends_action: true,
                        characters: 0,
                        delay_after: Duration::from_millis(u64::from(delay_ms)),
                    }));
                }
                InputAction::Wait { wait_ms } => {
                    steps.push_back(Step::Wait(Duration::from_millis(u64::from(wait_ms))))
                }
            }
        }
        ensure!(
            held.is_empty(),
            "sequence ends with held keys; add matching key up actions"
        );
        Ok(steps)
    }
}

fn push_press(steps: &mut VecDeque<Step>, keys: Vec<u32>, hold_ms: u32, delay_ms: u32) {
    let delay = Duration::from_millis(u64::from(delay_ms));
    if hold_ms == 0 {
        steps.push_back(Step::Input(InputStep {
            operation: Operation::Chord { keycodes: keys },
            ends_action: true,
            characters: 0,
            delay_after: delay,
        }));
        return;
    }
    steps.push_back(Step::Input(InputStep {
        operation: Operation::Keys {
            keycodes: keys.clone(),
            pressed: true,
        },
        ends_action: false,
        characters: 0,
        delay_after: Duration::from_millis(u64::from(hold_ms)),
    }));
    steps.push_back(Step::Input(InputStep {
        operation: Operation::Keys {
            keycodes: keys.into_iter().rev().collect(),
            pressed: false,
        },
        ends_action: true,
        characters: 0,
        delay_after: delay,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, state: KeyState, delay_ms: Option<u32>) -> InputAction {
        InputAction::Keys {
            keys: vec![name.into()],
            state,
            delay_ms,
            hold_ms: None,
        }
    }

    fn input(step: &Step) -> &InputStep {
        let Step::Input(input) = step else {
            panic!("expected input step")
        };
        input
    }

    #[test]
    fn plans_validate_the_whole_sequence_and_preserve_character_boundaries() {
        assert!(Plan::new(vec![InputAction::Text("a".repeat(26))], 1000, 0, 0).is_ok());
        assert!(Plan::new(vec![InputAction::Text("a".repeat(27))], 1000, 0, 0).is_err());
        assert!(
            Plan::new(
                vec![
                    InputAction::Text("prefix".into()),
                    key("a", KeyState::Press, None)
                ],
                0,
                0,
                0
            )
            .expect("plan")
            .resolve(vec![LinuxKeycode(0)])
            .is_err()
        );
        assert!(
            Plan::new(
                vec![
                    InputAction::Text("valid".into()),
                    InputAction::Text("bad\0".into())
                ],
                0,
                0,
                0
            )
            .is_err()
        );
        assert!(Plan::new(vec![InputAction::Text("é".repeat(2049))], 0, 0, 0).is_err());
        assert!(Plan::new(vec![InputAction::Text("ab".repeat(1000))], 25, 0, 0).is_err());
        assert!(
            Plan::new(
                vec![InputAction::Keys {
                    keys: vec!["Control".into(), "Ctrl".into()],
                    state: KeyState::Press,
                    delay_ms: None,
                    hold_ms: None,
                }],
                0,
                0,
                0
            )
            .expect("names")
            .resolve(vec![LinuxKeycode(29), LinuxKeycode(29)])
            .is_err()
        );
        let plan = Plan::new(
            vec![
                InputAction::Text("é🦀".into()),
                key("Return", KeyState::Press, None),
            ],
            25,
            0,
            0,
        )
        .expect("plan");
        let steps = plan.resolve(vec![LinuxKeycode(28)]).expect("resolved");
        assert_eq!(steps.len(), 3);
        assert!(matches!(&input(&steps[0]).operation, Operation::TypeText { text } if text == "é"));
        assert!(!input(&steps[0]).ends_action);
        assert_eq!(input(&steps[0]).delay_after, Duration::from_millis(25));
        assert!(
            matches!(&input(&steps[1]).operation, Operation::TypeText { text } if text == "🦀")
        );
        assert!(input(&steps[1]).ends_action);
        assert!(input(&steps[1]).delay_after.is_zero());
    }

    #[test]
    fn transitions_override_global_timing_and_waits_share_the_duration_budget() {
        let plan = Plan::new(
            vec![
                key("Control", KeyState::Down, Some(10)),
                key("a", KeyState::Press, None),
                InputAction::Wait { wait_ms: 200 },
                key("Control", KeyState::Up, Some(0)),
                key("Return", KeyState::Press, Some(15)),
            ],
            0,
            50,
            0,
        )
        .expect("plan");
        let steps = plan
            .resolve(vec![
                LinuxKeycode(29),
                LinuxKeycode(30),
                LinuxKeycode(29),
                LinuxKeycode(28),
            ])
            .expect("resolve");
        assert!(
            matches!(&input(&steps[0]).operation, Operation::Keys { keycodes, pressed: true } if keycodes == &[29])
        );
        assert_eq!(input(&steps[0]).delay_after, Duration::from_millis(10));
        assert_eq!(input(&steps[1]).delay_after, Duration::from_millis(50));
        assert!(matches!(steps[2], Step::Wait(duration) if duration == Duration::from_millis(200)));
        assert!(
            matches!(&input(&steps[3]).operation, Operation::Keys { keycodes, pressed: false } if keycodes == &[29])
        );
        assert_eq!(input(&steps[3]).delay_after, Duration::ZERO);
        assert_eq!(input(&steps[4]).delay_after, Duration::from_millis(15));
        assert!(Plan::new(vec![InputAction::Wait { wait_ms: 25_000 }], 0, 0, 0).is_ok());
        assert!(
            Plan::new(
                vec![
                    InputAction::Wait { wait_ms: 25_000 },
                    key("a", KeyState::Press, None)
                ],
                0,
                1,
                0
            )
            .is_err()
        );
        assert!(Plan::new(vec![key("a", KeyState::Press, Some(u32::MAX))], 0, 0, 0).is_err());
        assert!(
            Plan::new(
                vec![
                    InputAction::Text("ab".into()),
                    InputAction::Wait { wait_ms: 25_000 }
                ],
                1,
                0,
                0
            )
            .is_err()
        );
    }

    #[test]
    fn resolution_rejects_unbalanced_keys_alias_collisions_and_text_while_held() {
        for actions in [
            vec![key("Control", KeyState::Down, None)],
            vec![key("Control", KeyState::Up, None)],
            vec![
                key("Control", KeyState::Down, None),
                key("Ctrl", KeyState::Down, None),
            ],
            vec![
                key("Control", KeyState::Down, None),
                key("Ctrl", KeyState::Press, None),
            ],
            vec![
                key("Control", KeyState::Down, None),
                InputAction::Text("text".into()),
                key("Ctrl", KeyState::Up, None),
            ],
            vec![
                key("Control", KeyState::Down, None),
                key("Ctrl", KeyState::Press, None),
            ],
        ] {
            let plan = Plan::new(actions, 0, 0, 0).expect("syntax");
            let codes = vec![LinuxKeycode(29); plan.names.len()];
            assert!(plan.resolve(codes).is_err());
        }
    }

    #[test]
    fn compact_rows_plan_numeric_groups_named_keys_and_timed_presses() {
        let schema = serde_json::to_value(schemars::schema_for!(InputAction)).expect("schema");
        for row in schema["anyOf"].as_array().expect("action alternatives") {
            assert_eq!(row["minItems"], 2, "MCP must accept omitted timing fields");
        }
        let actions = serde_json::from_str(
            r#"[
            ["p",57], ["d",[29,"a"],0], ["w",20], ["u",["a",29]],
            ["p",[29,30],7,0], ["t","ok"]
        ]"#,
        )
        .expect("compact actions");
        let plan = Plan::new(actions, 0, 9, 5).expect("plan");
        assert_eq!(plan.names, ["a", "a"]);
        let steps = plan
            .resolve(vec![LinuxKeycode(30), LinuxKeycode(30)])
            .expect("resolved");
        assert_eq!(steps.len(), 8);
        assert!(
            matches!(&input(&steps[0]).operation, Operation::Keys { keycodes, pressed: true } if keycodes == &[57])
        );
        assert_eq!(input(&steps[0]).delay_after, Duration::from_millis(5));
        assert_eq!(input(&steps[1]).delay_after, Duration::from_millis(9));
        assert!(
            matches!(&input(&steps[2]).operation, Operation::Keys { keycodes, pressed: true } if keycodes == &[29,30])
        );
        assert!(matches!(steps[3], Step::Wait(duration) if duration == Duration::from_millis(20)));
        assert!(
            matches!(&input(&steps[4]).operation, Operation::Keys { keycodes, pressed: false } if keycodes == &[30,29])
        );
        assert_eq!(input(&steps[5]).delay_after, Duration::from_millis(7));
        assert!(
            matches!(&input(&steps[6]).operation, Operation::Keys { keycodes, pressed: false } if keycodes == &[30,29])
        );
        assert_eq!(input(&steps[6]).delay_after, Duration::ZERO);
        assert!(
            matches!(&input(&steps[7]).operation, Operation::TypeText { text } if text == "ok")
        );

        for json in [r#"[["p",0]]"#, r#"[["p",[]]]"#, r#"[["p",57,25001]]"#] {
            assert!(Plan::new(serde_json::from_str(json).expect("syntax"), 0, 0, 0).is_err());
        }
        let plan = Plan::new(
            serde_json::from_str(r#"[["p",[29,"Control"]]]"#).expect("syntax"),
            0,
            0,
            0,
        )
        .expect("plan");
        assert!(plan.resolve(vec![LinuxKeycode(29)]).is_err());
    }
}
