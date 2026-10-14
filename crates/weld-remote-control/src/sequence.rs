//! Validated keyboard sequences and bounded, deadline-paced execution plans.

use crate::{InputReceipt, Operation};
use anyhow::{Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};
use weld_client::{LinuxKeycode, TextInput};

/// A literal text string, a complete named keypress, or a complete named chord.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum InputAction {
    Text(String),
    Key { key: String },
    Chord { chord: Vec<String> },
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
    pub has_text: bool,
}

pub(crate) struct Step {
    pub operation: Operation,
    pub ends_action: bool,
    pub characters: usize,
    pub delay_after: Duration,
}

impl Plan {
    pub fn new(actions: Vec<InputAction>, typing_interval_ms: u32) -> Result<Self> {
        ensure!(
            !actions.is_empty() && actions.len() <= 64,
            "input needs 1 to 64 actions"
        );
        ensure!(
            typing_interval_ms <= 1000,
            "typing_interval_ms must be between 0 and 1000"
        );
        let mut names = Vec::new();
        let mut bytes = 0;
        let mut typing_gaps = 0;
        let mut has_text = false;
        for action in &actions {
            let keys = match action {
                InputAction::Text(text) => {
                    TextInput::try_from(text.clone()).map_err(anyhow::Error::msg)?;
                    bytes += text.len();
                    typing_gaps += text.chars().count().saturating_sub(1);
                    has_text = true;
                    continue;
                }
                InputAction::Key { key } => std::slice::from_ref(key),
                InputAction::Chord { chord } => {
                    ensure!(
                        !chord.is_empty() && chord.len() <= 16,
                        "chord needs 1 to 16 named keys"
                    );
                    chord.as_slice()
                }
            };
            for name in keys {
                ensure!(
                    !name.is_empty() && name.len() <= 64 && !name.contains('\0'),
                    "invalid key name"
                );
                names.push(name.clone());
            }
        }
        ensure!(
            bytes <= TextInput::MAX_BYTES,
            "combined text exceeds 4096 UTF-8 bytes"
        );
        ensure!(
            typing_gaps as u64 * u64::from(typing_interval_ms) <= 25_000,
            "typing duration exceeds 25 seconds; split the sequence or reduce the interval"
        );
        Ok(Self {
            actions,
            names,
            interval: Duration::from_millis(u64::from(typing_interval_ms)),
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
        for action in self.actions {
            match action {
                InputAction::Text(text) if self.interval.is_zero() => steps.push_back(Step {
                    characters: text.chars().count(),
                    operation: Operation::TypeText { text },
                    ends_action: true,
                    delay_after: Duration::ZERO,
                }),
                InputAction::Text(text) => {
                    let mut chars = text.chars().peekable();
                    while let Some(character) = chars.next() {
                        let ends_action = chars.peek().is_none();
                        steps.push_back(Step {
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
                        });
                    }
                }
                action @ (InputAction::Key { .. } | InputAction::Chord { .. }) => {
                    let count = if let InputAction::Chord { chord } = action {
                        chord.len()
                    } else {
                        1
                    };
                    let keys: Vec<_> = codes.by_ref().take(count).map(|key| key.0).collect();
                    ensure!(
                        keys.len() == count
                            && keys.iter().copied().collect::<HashSet<_>>().len() == count,
                        "chord resolves to duplicate keys"
                    );
                    steps.push_back(Step {
                        operation: Operation::Chord { keycodes: keys },
                        ends_action: true,
                        characters: 0,
                        delay_after: Duration::ZERO,
                    });
                }
            }
        }
        Ok(steps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_validate_the_whole_sequence_and_preserve_character_boundaries() {
        assert!(Plan::new(vec![InputAction::Text("a".repeat(26))], 1000).is_ok());
        assert!(Plan::new(vec![InputAction::Text("a".repeat(27))], 1000).is_err());
        assert!(
            Plan::new(
                vec![
                    InputAction::Text("prefix".into()),
                    InputAction::Key { key: "a".into() }
                ],
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
                0
            )
            .is_err()
        );
        assert!(Plan::new(vec![InputAction::Text("é".repeat(2049))], 0).is_err());
        assert!(Plan::new(vec![InputAction::Text("ab".repeat(1000))], 25).is_err());
        assert!(
            Plan::new(
                vec![InputAction::Chord {
                    chord: vec!["Control".into(), "Ctrl".into()]
                }],
                0
            )
            .expect("names")
            .resolve(vec![LinuxKeycode(29), LinuxKeycode(29)])
            .is_err()
        );
        let plan = Plan::new(
            vec![
                InputAction::Text("é🦀".into()),
                InputAction::Key {
                    key: "Return".into(),
                },
            ],
            25,
        )
        .expect("plan");
        let steps = plan.resolve(vec![LinuxKeycode(28)]).expect("resolved");
        assert_eq!(steps.len(), 3);
        assert!(matches!(&steps[0].operation, Operation::TypeText { text } if text == "é"));
        assert!(!steps[0].ends_action);
        assert_eq!(steps[0].delay_after, Duration::from_millis(25));
        assert!(matches!(&steps[1].operation, Operation::TypeText { text } if text == "🦀"));
        assert!(steps[1].ends_action);
        assert!(steps[1].delay_after.is_zero());
    }
}
