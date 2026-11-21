// Interactive prompts (the original uses `@clack/prompts`); leaving one ends the command like `UICancelledError`.
// Text and password prompts take one line from stdin when it is piped, so logins can be scripted.
use crate::ui;
use anyhow::Result;
use inquire::validator::Validation;
use inquire::{Confirm, InquireError, MultiSelect, Password, PasswordDisplayMode, Select, Text};
use std::io::IsTerminal;

pub struct Choice {
    pub label: String,
    pub hint: Option<String>,
}

impl Choice {
    pub fn new(label: impl Into<String>) -> Self {
        Choice { label: label.into(), hint: None }
    }

    pub fn hint(label: impl Into<String>, hint: impl Into<String>) -> Self {
        Choice { label: label.into(), hint: Some(hint.into()) }
    }
}

impl std::fmt::Display for Choice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.hint {
            Some(hint) => write!(f, "{} ({hint})", self.label),
            None => f.write_str(&self.label),
        }
    }
}

// Prompts read stdin and draw on stderr; both must be a terminal, or a redirected run waits on a prompt nobody sees.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

fn settle<T>(result: std::result::Result<T, InquireError>) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted | InquireError::NotTTY) => Err(ui::cancelled()),
        Err(error) => Err(error.into()),
    }
}

fn piped_line() -> Result<String> {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim_end_matches(['\r', '\n']).to_string();
    if line.is_empty() { Err(ui::cancelled()) } else { Ok(line) }
}

// The index of the chosen option; typing filters the list.
pub fn select(message: &str, choices: Vec<Choice>, start: usize, page: usize) -> Result<usize> {
    if !interactive() {
        return Err(ui::cancelled());
    }
    let prompt = Select::new(message, choices).with_starting_cursor(start).with_page_size(page.max(1));
    Ok(settle(prompt.raw_prompt())?.index)
}

pub fn text(message: &str, placeholder: Option<&str>, validate: Option<fn(&str) -> Option<&'static str>>) -> Result<String> {
    if !interactive() {
        return piped_line();
    }
    let mut prompt = Text::new(message);
    if let Some(placeholder) = placeholder {
        prompt = prompt.with_placeholder(placeholder);
    }
    if let Some(check) = validate {
        prompt = prompt.with_validator(move |input: &str| Ok(check(input).map_or(Validation::Valid, |reason| Validation::Invalid(reason.into()))));
    }
    settle(prompt.prompt())
}

pub fn password(message: &str) -> Result<String> {
    if !interactive() {
        return piped_line();
    }
    let required = |input: &str| Ok(if input.is_empty() { Validation::Invalid("Required".into()) } else { Validation::Valid });
    settle(Password::new(message).without_confirmation().with_display_mode(PasswordDisplayMode::Masked).with_validator(required).prompt())
}

pub fn confirm(message: &str, initial: bool) -> Result<bool> {
    if !interactive() {
        return Err(ui::cancelled());
    }
    settle(Confirm::new(message).with_default(initial).prompt())
}

// The chosen labels; everything starts selected.
pub fn multiselect(message: &str, options: &[&str]) -> Result<Vec<String>> {
    if !interactive() {
        return Err(ui::cancelled());
    }
    let prompt = MultiSelect::new(message, options.to_vec()).with_all_selected_by_default();
    Ok(settle(prompt.prompt())?.into_iter().map(str::to_string).collect())
}
