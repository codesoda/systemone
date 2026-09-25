//! The questions `s1 setup` asks, behind a small trait so every flow can be
//! tested with scripted answers.

#[cfg(test)]
use std::collections::VecDeque;

use crate::CliError;

pub trait Prompter {
    /// Pick one of `items`; returns its index.
    fn select(&mut self, prompt: &str, items: &[String], default: usize)
    -> Result<usize, CliError>;
    /// Free text with a default.
    fn input(&mut self, prompt: &str, default: &str) -> Result<String, CliError>;
    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool, CliError>;
    /// Information for the user (stderr).
    fn say(&mut self, text: &str);
}

fn cancelled() -> CliError {
    CliError::runtime("cancelled", "setup cancelled; nothing was written")
}

/// Interactive prompts on the terminal's stderr.
pub struct TerminalPrompter {
    term: dialoguer::console::Term,
    theme: dialoguer::theme::ColorfulTheme,
}

impl TerminalPrompter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            term: dialoguer::console::Term::stderr(),
            theme: dialoguer::theme::ColorfulTheme::default(),
        }
    }
}

impl Default for TerminalPrompter {
    fn default() -> Self {
        Self::new()
    }
}

fn io_error(error: &dialoguer::Error) -> CliError {
    CliError::runtime("terminal_io", error.to_string())
}

impl Prompter for TerminalPrompter {
    fn select(
        &mut self,
        prompt: &str,
        items: &[String],
        default: usize,
    ) -> Result<usize, CliError> {
        dialoguer::Select::with_theme(&self.theme)
            .with_prompt(prompt)
            .items(items)
            .default(default)
            .interact_on_opt(&self.term)
            .map_err(|error| io_error(&error))?
            .ok_or_else(cancelled)
    }

    fn input(&mut self, prompt: &str, default: &str) -> Result<String, CliError> {
        let mut input = dialoguer::Input::<String>::with_theme(&self.theme).with_prompt(prompt);
        if !default.is_empty() {
            input = input.default(default.to_owned());
        }
        input
            .interact_text_on(&self.term)
            .map(|text| text.trim().to_owned())
            .map_err(|error| io_error(&error))
    }

    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool, CliError> {
        dialoguer::Confirm::with_theme(&self.theme)
            .with_prompt(prompt)
            .default(default)
            .interact_on_opt(&self.term)
            .map_err(|error| io_error(&error))?
            .ok_or_else(cancelled)
    }

    fn say(&mut self, text: &str) {
        let _ = self.term.write_line(text);
    }
}

/// Takes the default for every question. Used by `--yes`, which is also the
/// only way to run setup without a terminal.
pub struct DefaultsPrompter<W: std::io::Write> {
    output: W,
}

impl<W: std::io::Write> DefaultsPrompter<W> {
    pub const fn new(output: W) -> Self {
        Self { output }
    }
}

impl<W: std::io::Write> Prompter for DefaultsPrompter<W> {
    fn select(
        &mut self,
        prompt: &str,
        items: &[String],
        default: usize,
    ) -> Result<usize, CliError> {
        let choice = items.get(default).map_or("", String::as_str);
        let _ = writeln!(self.output, "{prompt}: {choice}");
        Ok(default)
    }

    fn input(&mut self, prompt: &str, default: &str) -> Result<String, CliError> {
        let _ = writeln!(self.output, "{prompt}: {default}");
        Ok(default.to_owned())
    }

    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool, CliError> {
        let _ = writeln!(
            self.output,
            "{prompt}: {}",
            if default { "yes" } else { "no" }
        );
        Ok(default)
    }

    fn say(&mut self, text: &str) {
        let _ = writeln!(self.output, "{text}");
    }
}

/// One scripted answer.
#[cfg(test)]
#[derive(Clone, Debug)]
pub enum Answer {
    /// Select the item whose label starts with this text.
    Pick(&'static str),
    Text(&'static str),
    Yes,
    No,
    /// Accept the default of whatever is asked.
    Accept,
}

/// Replays answers in order and records everything shown. Fails the flow if
/// it asks more questions than scripted or the wrong kind of question.
#[cfg(test)]
pub struct ScriptedPrompter {
    answers: VecDeque<Answer>,
    pub transcript: Vec<String>,
}

#[cfg(test)]
impl ScriptedPrompter {
    #[must_use]
    pub fn new(answers: &[Answer]) -> Self {
        Self {
            answers: answers.iter().cloned().collect(),
            transcript: Vec::new(),
        }
    }

    fn next(&mut self, prompt: &str) -> Result<Answer, CliError> {
        self.answers.pop_front().ok_or_else(|| {
            CliError::runtime(
                "script_exhausted",
                format!("no scripted answer for {prompt:?}"),
            )
        })
    }

    #[must_use]
    pub fn remaining(&self) -> usize {
        self.answers.len()
    }

    #[must_use]
    pub fn shown(&self) -> String {
        self.transcript.join("\n")
    }
}

#[cfg(test)]
impl Prompter for ScriptedPrompter {
    fn select(
        &mut self,
        prompt: &str,
        items: &[String],
        default: usize,
    ) -> Result<usize, CliError> {
        self.transcript
            .push(format!("? {prompt} [{}]", items.join(" | ")));
        match self.next(prompt)? {
            Answer::Accept => Ok(default),
            Answer::Pick(prefix) => items
                .iter()
                .position(|item| item.starts_with(prefix))
                .ok_or_else(|| {
                    CliError::runtime(
                        "script_mismatch",
                        format!("{prompt:?}: no item starts with {prefix:?} in {items:?}"),
                    )
                }),
            other => Err(CliError::runtime(
                "script_mismatch",
                format!("{prompt:?} is a selection, scripted {other:?}"),
            )),
        }
    }

    fn input(&mut self, prompt: &str, default: &str) -> Result<String, CliError> {
        self.transcript
            .push(format!("? {prompt} (default {default:?})"));
        match self.next(prompt)? {
            Answer::Accept => Ok(default.to_owned()),
            Answer::Text(text) => Ok(text.to_owned()),
            other => Err(CliError::runtime(
                "script_mismatch",
                format!("{prompt:?} is text input, scripted {other:?}"),
            )),
        }
    }

    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool, CliError> {
        self.transcript
            .push(format!("? {prompt} (default {default})"));
        match self.next(prompt)? {
            Answer::Accept => Ok(default),
            Answer::Yes => Ok(true),
            Answer::No => Ok(false),
            other => Err(CliError::runtime(
                "script_mismatch",
                format!("{prompt:?} is yes/no, scripted {other:?}"),
            )),
        }
    }

    fn say(&mut self, text: &str) {
        self.transcript.push(text.to_owned());
    }
}
