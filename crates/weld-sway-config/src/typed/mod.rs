//! Typed Sway syntax after ordered variable expansion.
//!
//! Generated productions own command spelling and local argument constraints.
//! This module assembles blocks, preserves shell payloads and records source
//! locations. Native capability checks and resource preparation belong to the
//! configuration adapter.

mod generated;
pub use generated::*;
#[cfg(test)]
mod tests;

use crate::{
    Statement,
    evaluation::{expand_variables, line_of},
};
use anyhow::{Context, Result, bail, ensure};
use std::num::NonZeroU32;
use weld_config::{ConfigWarning, Located, Location, UnsupportedPolicy, unsupported};

trait Word: Sized {
    fn word(word: &str) -> Result<Self>;
}
trait Words: Sized {
    fn words(words: &[&str]) -> Result<Self>;
}

fn literal(value: &str) -> Result<&str> {
    crate::value::parse(value)
}
/// Parsed configuration and explicitly skipped syntax.
#[derive(Clone, Debug, Default)]
pub struct Document {
    /// Declarations in source order.
    pub directives: Vec<Located<Directive>>,
    /// Features skipped under the selected compatibility policy.
    pub warnings: Vec<ConfigWarning>,
}

/// Supported statement shapes. Native execution support is checked when lowering.
#[derive(Clone, Debug, PartialEq)]
pub enum Directive {
    /// The setting form.
    Setting(Setting),
    /// The binding form.
    Binding(Binding),
    /// The mode form.
    Mode(ModeDefinition),
    /// The input form.
    Input(InputSelector, InputProperty),
    /// The cursor form.
    Cursor(Literal, Option<PositiveInteger>),
    /// The output scale form.
    OutputScale(Literal, FiniteFloat),
    /// The startup form.
    Startup {
        /// Typed command or preserved shell payload.
        command: ShellCommand,
        /// Whether successful reloads also execute this startup command.
        on_reload: bool,
    },
    /// The rule form.
    Rule(WindowRule),
}

/// Modifier requirements, with duplicate modifiers rejected at parsing.
#[derive(Clone, Debug, PartialEq)]
pub struct Modifiers(pub Vec<Modifier>);
/// A decoded literal whose quotes are no longer part of its value.
#[derive(Clone, Debug, PartialEq)]
pub struct Literal(pub String);
impl Word for Literal {
    fn word(word: &str) -> Result<Self> {
        Ok(Self(literal(word)?.to_owned()))
    }
}
struct Literals(Vec<Literal>);
impl Words for Literals {
    fn words(words: &[&str]) -> Result<Self> {
        ensure!(!words.is_empty(), "expected at least one literal");
        Ok(Self(
            words
                .iter()
                .map(|word| Literal::word(word))
                .collect::<Result<_>>()?,
        ))
    }
}
/// Finite floating-point syntax, before native range validation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FiniteFloat(pub f64);
impl Word for FiniteFloat {
    fn word(word: &str) -> Result<Self> {
        let value: f64 = literal(word)?.parse()?;
        ensure!(value.is_finite(), "expected finite number");
        Ok(Self(value))
    }
}
/// Positive cursor size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositiveInteger(pub NonZeroU32);
impl Word for PositiveInteger {
    fn word(word: &str) -> Result<Self> {
        Ok(Self(
            NonZeroU32::new(literal(word)?.parse()?).context("size must be positive")?,
        ))
    }
}
/// Preserved shell text; parsing does not execute or expand shell expressions.
#[derive(Clone, Debug, PartialEq)]
pub struct ShellCommand(pub String);
impl Words for ShellCommand {
    fn words(words: &[&str]) -> Result<Self> {
        let words = words.strip_prefix(&["--no-startup-id"]).unwrap_or(words);
        ensure!(!words.is_empty(), "exec requires a shell command");
        if let [word] = words {
            for quote in ['\'', '"'] {
                if let Some(value) = word.strip_prefix(quote).and_then(|s| s.strip_suffix(quote)) {
                    ensure!(!value.trim().is_empty(), "exec requires a shell command");
                    return Ok(Self(value.to_owned()));
                }
            }
        }
        Ok(Self(words.join(" ")))
    }
}
/// Pango description text, decoded before native typography preparation.
#[derive(Clone, Debug, PartialEq)]
pub struct FontDescription(pub String);
impl Words for FontDescription {
    fn words(words: &[&str]) -> Result<Self> {
        Ok(Self(
            words
                .iter()
                .map(|word| literal(word))
                .collect::<Result<Vec<_>>>()?
                .join(" "),
        ))
    }
}
/// XKB-name syntax. Native keymap compilation happens during preparation.
#[derive(Clone, Debug, PartialEq)]
pub struct XkbValue(pub String);
impl Word for XkbValue {
    fn word(word: &str) -> Result<Self> {
        let value = if let Some(quote) = word.chars().next().filter(|c| matches!(c, '\'' | '"')) {
            word.strip_prefix(quote)
                .and_then(|value| value.strip_suffix(quote))
                .context("unterminated XKB value")?
        } else {
            ensure!(!word.is_empty(), "missing XKB value");
            word
        };
        ensure!(
            value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-,:+/.()".contains(c)),
            "invalid literal XKB value"
        );
        Ok(Self(value.to_owned()))
    }
}
impl Word for Modifiers {
    fn word(word: &str) -> Result<Self> {
        let mut values = Vec::new();
        for word in word.split('+') {
            ensure!(!word.contains('$'), "unresolved variable in modifier");
            let modifier = Modifier::word(word)?;
            ensure!(!values.contains(&modifier), "duplicate binding modifier");
            values.push(modifier);
        }
        Ok(Self(values))
    }
}
/// Binding spelling retained until lowering normalizes Linux physical identity.
#[derive(Clone, Debug, PartialEq)]
pub enum BindingKey {
    /// A supported key name, using Weld's existing physical-position semantics.
    Named(Key),
    /// An XKB keycode (Linux evdev code plus eight).
    Code(XkbKeycode),
}

#[derive(Clone, Copy)]
enum BindingKind {
    Named,
    Code,
}

impl Word for Chord {
    fn word(word: &str) -> Result<Self> {
        Self::parse(word, BindingKind::Named)
    }
}
impl Chord {
    fn parse(word: &str, kind: BindingKind) -> Result<Self> {
        ensure!(
            !word.is_empty() && !word.chars().any(char::is_whitespace),
            "invalid binding chord"
        );
        let (prefix, trigger) = word
            .rsplit_once('+')
            .map_or((None, word), |(prefix, key)| (Some(prefix), key));
        ensure!(
            !trigger.is_empty() && !trigger.contains('$'),
            "invalid or unresolved binding key"
        );
        let mut modifiers = prefix
            .map(Modifiers::word)
            .transpose()?
            .unwrap_or(Modifiers(Vec::new()));
        let key = if matches!(kind, BindingKind::Code) {
            Some(BindingKey::Code(XkbKeycode::word(trigger)?))
        } else if let Ok(modifier) = Modifier::word(trigger) {
            ensure!(
                !modifiers.0.contains(&modifier),
                "duplicate binding modifier"
            );
            modifiers.0.push(modifier);
            None
        } else {
            Some(BindingKey::Named(Key::word(trigger)?))
        };
        Ok(Self { modifiers, key })
    }
}

/// Workspace reference retains named and numbered targeting distinctions.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceTarget {
    /// The name form.
    Name(WorkspaceName),
    /// The number form.
    Number(WorkspaceName),
    /// The next form.
    Next,
    /// The previous form.
    Previous,
    /// The next on output form.
    NextOnOutput,
    /// The previous on output form.
    PreviousOnOutput,
    /// The back and forth form.
    BackAndForth,
    /// The current form.
    Current,
}
fn workspace_name(words: &[&str]) -> Result<WorkspaceName> {
    let name = words
        .iter()
        .map(|word| literal(word))
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    ensure!(!name.starts_with("--"), "unsupported workspace name");
    WorkspaceName::new(name)
}
impl Words for WorkspaceTarget {
    fn words(words: &[&str]) -> Result<Self> {
        Ok(match words {
            ["number", rest @ ..] => {
                let name = workspace_name(rest)?;
                let digits: String = name
                    .as_str()
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                ensure!(
                    digits.parse::<i32>().is_ok(),
                    "workspace number requires a leading nonnegative integer"
                );
                Self::Number(name)
            }
            ["next"] => Self::Next,
            ["prev"] => Self::Previous,
            ["next_on_output"] => Self::NextOnOutput,
            ["prev_on_output"] => Self::PreviousOnOutput,
            ["back_and_forth"] => Self::BackAndForth,
            ["current"] => Self::Current,
            _ => Self::Name(workspace_name(words)?),
        })
    }
}

impl Words for RenameWorkspace {
    fn words(words: &[&str]) -> Result<Self> {
        let separator = words
            .iter()
            .position(|word| *word == "to")
            .context("rename workspace requires to NEW_NAME")?;
        Ok(Self {
            source: (separator > 0)
                .then(|| workspace_name(&words[..separator]))
                .transpose()?,
            name: workspace_name(&words[separator + 1..])?,
        })
    }
}

/// Layout alternatives preserve split restoration versus explicit orientations.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LayoutChoice {
    /// The horizontal form.
    Horizontal,
    /// The vertical form.
    Vertical,
    /// The tabbed form.
    Tabbed,
    /// The stacked form.
    Stacked,
    /// The split form.
    Split,
}
impl Word for LayoutChoice {
    fn word(word: &str) -> Result<Self> {
        Ok(match word {
            "splith" => Self::Horizontal,
            "splitv" => Self::Vertical,
            "tabbed" => Self::Tabbed,
            "stacking" | "stacked" => Self::Stacked,
            "split" => Self::Split,
            _ => bail!("unknown layout {word}"),
        })
    }
}
/// Existing-layout requests differ from preparing a split for the next window.
#[derive(Clone, Debug, PartialEq)]
pub enum Layout {
    /// The set form.
    Set(LayoutChoice),
    /// The default form.
    Default,
    /// The toggle form.
    Toggle,
    /// The toggle all form.
    ToggleAll,
    /// The cycle form.
    Cycle(Vec<LayoutChoice>),
}
impl Words for Layout {
    fn words(words: &[&str]) -> Result<Self> {
        Ok(match words {
            ["default"] => Self::Default,
            ["toggle"] => Self::Toggle,
            ["toggle", "all"] => Self::ToggleAll,
            ["toggle", choices @ ..] if !choices.is_empty() => Self::Cycle(
                choices
                    .iter()
                    .map(|v| LayoutChoice::word(v))
                    .collect::<Result<_>>()?,
            ),
            [word] => {
                let value = LayoutChoice::word(word)?;
                ensure!(
                    value != LayoutChoice::Split,
                    "use layout toggle split or layout default"
                );
                Self::Set(value)
            }
            _ => bail!("layout requires a name or toggle sequence"),
        })
    }
}

impl Words for Persistence {
    fn words(words: &[&str]) -> Result<Self> {
        let (operation, field) = match words {
            [operation] => (Toggle::word(operation)?, PersistenceField::Auto),
            [operation, field] => (Toggle::word(operation)?, PersistenceField::word(field)?),
            _ => bail!("expected persistence operation and optional field"),
        };
        ensure!(
            operation != Toggle::Disable || words.len() == 1,
            "disable does not accept an identity selector"
        );
        Ok(Self { operation, field })
    }
}

impl Words for Resize {
    fn words(words: &[&str]) -> Result<Self> {
        let [kind, dimension, rest @ ..] = words else {
            bail!("resize requires kind, dimension and amount")
        };
        let pairs = match rest {
            [amount, unit] => vec![(*amount, *unit)],
            [a, u, "or", b, v] => vec![(*a, *u), (*b, *v)],
            _ => return Err(unsupported("resize requires amounts with px or ppt")),
        };
        let mut amounts: Vec<ResizeAmount> = Vec::new();
        for (amount, unit) in pairs {
            let value = amount.parse::<u16>()?;
            let unit = Unit::word(unit)?;
            ensure!(
                value > 0 && (unit != Unit::Percentage || value <= 100),
                "invalid resize amount"
            );
            ensure!(
                !amounts.iter().any(|amount| amount.unit == unit),
                "duplicate resize unit"
            );
            amounts.push(ResizeAmount { value, unit });
        }
        Ok(Self {
            kind: ResizeKind::word(kind)?,
            dimension: Dimension::word(dimension)?,
            amounts,
        })
    }
}

/// Encoded RGBA channels in configuration color space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color(pub [u8; 4]);
impl Word for Color {
    fn word(word: &str) -> Result<Self> {
        ensure!(
            word.starts_with('#') && matches!(word.len(), 7 | 9) && word.is_ascii(),
            "colors must use #RRGGBB or #RRGGBBAA"
        );
        let mut channels = [255; 4];
        for (index, channel) in channels.iter_mut().enumerate().take((word.len() - 1) / 2) {
            *channel = u8::from_str_radix(&word[1 + index * 2..3 + index * 2], 16)?;
        }
        Ok(Self(channels))
    }
}

impl Words for Colors {
    fn words(words: &[&str]) -> Result<Self> {
        ensure!(
            (3..=5).contains(&words.len()),
            "client colors require three to five colors"
        );
        let values = words
            .iter()
            .map(|word| Color::word(word))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            border: values[0],
            background: values[1],
            text: values[2],
            indicator: values.get(3).copied().unwrap_or(values[0]),
            child_border: values.get(4).copied().unwrap_or(values[1]),
        })
    }
}
/// Input selector spelling classified independently of available devices.
#[derive(Clone, Debug, PartialEq)]
pub enum InputSelector {
    /// The all form.
    All,
    /// The keyboard form.
    Keyboard,
    /// The touchpad form.
    Touchpad,
    /// The identifier form.
    Identifier(String),
    /// The other type form.
    OtherType(String),
}
impl Word for InputSelector {
    fn word(word: &str) -> Result<Self> {
        let value = literal(word)?;
        ensure!(!value.is_empty(), "input selector must not be empty");
        Ok(match value {
            "*" => Self::All,
            "type:keyboard" => Self::Keyboard,
            "type:touchpad" => Self::Touchpad,
            value if value.starts_with("type:") => Self::OtherType(value.into()),
            value => Self::Identifier(value.into()),
        })
    }
}

/// Rule timing distinguishes initial assignment from once-per-match actions.
#[derive(Clone, Debug, PartialEq)]
pub enum RuleEffect {
    /// The commands form.
    Commands(Vec<Command>),
    /// The assign form.
    Assign(WorkspaceTarget),
    /// The no focus form.
    NoFocus,
}
fn criteria(value: &str) -> Result<Vec<Criterion>> {
    crate::criteria::parse(value)?
        .into_iter()
        .filter(|field| !(field.name == "all" && field.value.is_none()))
        .map(|field| {
            let kind = CriterionField::word(&field.name)?;
            ensure!(kind != CriterionField::All, "all does not accept a value");
            let pattern = field.value.context("window criterion requires a value")?;
            if pattern == "__focused__" {
                return Err(unsupported(
                    "dynamic __focused__ criteria are not supported",
                ));
            }
            Ok(Criterion {
                field: kind,
                pattern,
            })
        })
        .collect()
}

fn location(statement: &Statement, name: &str, source: &str) -> Result<Location> {
    let start = statement.header().segments().first().map_or(0, |s| s.start);
    let end = statement
        .header()
        .segments()
        .last()
        .map_or(start, |s| s.end);
    Ok(Location {
        source: name.to_owned(),
        line: line_of(source, statement)?,
        range: start..end,
    })
}
fn binding(statement: &Statement, kind: BindingKind) -> Result<Binding> {
    ensure!(
        statement.block().is_none(),
        "binding requires a command, not a block"
    );
    let words = statement
        .arguments()
        .iter()
        .map(|word| word.text())
        .collect::<Vec<_>>();
    let [chord, command @ ..] = words.as_slice() else {
        bail!("missing binding chord")
    };
    if chord.starts_with("--") {
        return Err(unsupported(format!(
            "unsupported binding flag {chord}; binding skipped"
        )));
    }
    ensure!(!command.is_empty(), "missing binding command");
    Ok(Binding {
        chord: Chord::parse(chord, kind)?,
        command: Command::words(command)?,
    })
}

/// Expand and parse a candidate without compiling keymaps or executing effects.
pub fn parse(name: &str, source: &str, policy: UnsupportedPolicy) -> Result<Document> {
    let expanded = expand_variables(name, source)?;
    let syntax = crate::parse_source(name, expanded.as_str())?;
    let mut document = Document::default();
    for statement in syntax.statements() {
        if let Err(error) =
            statement_nodes(statement, name, expanded.as_str(), policy, &mut document)
        {
            policy.handle(
                error.context(statement.header().text().to_owned()),
                name,
                line_of(expanded.as_str(), statement)?,
                &mut document.warnings,
            )?;
        }
    }
    document.warnings.sort_by_key(|warning| warning.line);
    Ok(document)
}

fn statement_nodes(
    statement: &Statement,
    name: &str,
    source: &str,
    policy: UnsupportedPolicy,
    document: &mut Document,
) -> Result<()> {
    let origin = location(statement, name, source)?;
    let words = statement
        .arguments()
        .iter()
        .map(|word| word.text())
        .collect::<Vec<_>>();
    let header = Header::word(statement.name().text());
    let value = match header {
        Ok(Header::Include) => {
            bail!("include is not supported yet; load a combined configuration file")
        }
        Ok(Header::Binding) => Directive::Binding(binding(statement, BindingKind::Named)?),
        Ok(Header::BindingCode) => Directive::Binding(binding(statement, BindingKind::Code)?),
        Ok(Header::Mode) if statement.block().is_some() => {
            let header = ModeHeader::words(&words)?;
            let mut bindings = Vec::new();
            if let Some(block) = statement.block() {
                for child in block.statements() {
                    let child_origin = location(child, name, source)?;
                    let result = match Header::word(child.name().text()) {
                        Ok(Header::Binding) => binding(child, BindingKind::Named),
                        Ok(Header::BindingCode) => binding(child, BindingKind::Code),
                        _ => Err(unsupported("unsupported mode directive")),
                    };
                    match result {
                        Ok(value) => bindings.push(Located {
                            value,
                            location: child_origin,
                        }),
                        Err(error) => {
                            policy.handle(error, name, child_origin.line, &mut document.warnings)?
                        }
                    }
                }
            }
            Directive::Mode(ModeDefinition { header, bindings })
        }
        Ok(Header::Input | Header::Seat | Header::Output) => {
            let header = header?;
            let [selector, rest @ ..] = words.as_slice() else {
                bail!("missing selector")
            };
            if let Some(block) = statement.block() {
                ensure!(rest.is_empty(), "unexpected block arguments");
                for child in block.statements() {
                    let child_origin = location(child, name, source)?;
                    let property = std::iter::once(child.name().text())
                        .chain(child.arguments().iter().map(|w| w.text()))
                        .take_while(|w| !w.starts_with('#'))
                        .collect::<Vec<_>>();
                    let result = if child.block().is_some() {
                        Err(unsupported("nested property blocks are not supported"))
                    } else {
                        property_nodes(
                            header,
                            selector,
                            &property,
                            child_origin.clone(),
                            policy,
                            document,
                        )
                    };
                    if let Err(error) = result {
                        policy.handle(error, name, child_origin.line, &mut document.warnings)?;
                    }
                }
            } else {
                let rest = rest
                    .iter()
                    .copied()
                    .take_while(|w| !w.starts_with('#'))
                    .collect::<Vec<_>>();
                property_nodes(header, selector, &rest, origin, policy, document)?;
            }
            return Ok(());
        }
        Ok(Header::Bar) if statement.block().is_some() => {
            if !words.is_empty() {
                return Err(unsupported("named bars are not supported"));
            }
            let mut command = None;
            if let Some(block) = statement.block() {
                for child in block.statements() {
                    if matches!(Header::word(child.name().text()), Ok(Header::BarCommand))
                        && child.block().is_none()
                    {
                        command = Some(ShellCommand::words(
                            &child
                                .arguments()
                                .iter()
                                .map(|w| w.text())
                                .collect::<Vec<_>>(),
                        )?);
                    } else {
                        policy.handle(
                            unsupported("unsupported bar setting"),
                            name,
                            line_of(source, child)?,
                            &mut document.warnings,
                        )?;
                    }
                }
            }
            let Some(command) = command else {
                return Err(unsupported(
                    "bar requires an explicit swaybar_command; no default bar was started",
                ));
            };
            Directive::Startup {
                command,
                on_reload: false,
            }
        }
        Ok(Header::Exec | Header::ExecAlways) => {
            ensure!(statement.block().is_none(), "exec requires a command");
            Directive::Startup {
                command: ShellCommand::words(&words)?,
                on_reload: matches!(header, Ok(Header::ExecAlways)),
            }
        }
        Ok(Header::ForWindow | Header::Assign | Header::NoFocus) => {
            let [selector, rest @ ..] = words.as_slice() else {
                bail!("missing window criteria")
            };
            let criteria = criteria(selector)?;
            let effect = match header? {
                Header::ForWindow => RuleEffect::Commands(
                    crate::commands::parse(&rest.join(" "))?
                        .iter()
                        .map(|words| Command::words(words))
                        .collect::<Result<_>>()?,
                ),
                Header::Assign => {
                    let rest = rest.strip_prefix(&["→"]).unwrap_or(rest);
                    if rest.first() == Some(&"output") {
                        return Err(unsupported("assign output is not supported"));
                    }
                    RuleEffect::Assign(WorkspaceTarget::words(
                        rest.strip_prefix(&["workspace"]).unwrap_or(rest),
                    )?)
                }
                _ => {
                    ensure!(rest.is_empty(), "no_focus requires criteria only");
                    RuleEffect::NoFocus
                }
            };
            Directive::Rule(WindowRule { criteria, effect })
        }
        _ => {
            if statement.block().is_some() {
                return Err(unsupported("unsupported configuration block"));
            }
            Directive::Setting(Setting::words(
                &std::iter::once(statement.name().text())
                    .chain(words.iter().copied())
                    .collect::<Vec<_>>(),
            )?)
        }
    };
    document.directives.push(Located {
        value,
        location: origin,
    });
    Ok(())
}

fn property_nodes(
    header: Header,
    selector: &str,
    words: &[&str],
    origin: Location,
    policy: UnsupportedPolicy,
    document: &mut Document,
) -> Result<()> {
    let value = match header {
        Header::Input => {
            ensure!(words.len() == 2, "input directive requires one value");
            Directive::Input(InputSelector::word(selector)?, InputProperty::words(words)?)
        }
        Header::Seat => {
            if literal(selector)? != "*" {
                return Err(unsupported("cursor settings currently support seat *"));
            }
            let SeatProperty::Cursor(theme, size) = SeatProperty::words(words)?;
            ensure!(!theme.0.trim().is_empty(), "cursor theme must not be empty");
            Directive::Cursor(theme, size)
        }
        Header::Output => {
            let selector = Literal::word(selector)?;
            ensure!(!selector.0.is_empty(), "output selector must not be empty");
            ensure!(!words.is_empty(), "output requires a property");
            let mut words = words.iter().copied().peekable();
            while let Some(property) = words.next() {
                let Ok(kind) = OutputPropertyKind::word(property) else {
                    policy.handle(
                        unsupported(format!("unsupported output property {property}")),
                        &origin.source,
                        origin.line,
                        &mut document.warnings,
                    )?;
                    break;
                };
                if !kind.is_supported() {
                    policy.handle(
                        unsupported(format!("unsupported output property {property}")),
                        &origin.source,
                        origin.line,
                        &mut document.warnings,
                    )?;
                }
                match kind.parse_next(&mut words)? {
                    OutputProperty::Scale(factor) => document.directives.push(Located {
                        value: Directive::OutputScale(selector.clone(), factor),
                        location: origin.clone(),
                    }),
                    OutputProperty::Skipped(_) => {}
                }
            }
            return Ok(());
        }
        _ => bail!("expected a property directive"),
    };
    document.directives.push(Located {
        value,
        location: origin,
    });
    Ok(())
}
