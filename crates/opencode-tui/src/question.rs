use crate::editor::Editor;
use crate::keybind::KeyStroke;
use opencode_session::{QuestionInfo, QuestionRequest};

#[derive(Debug, Clone, PartialEq)]
pub struct QuestionAsk {
    pub id: u64,
    pub request: QuestionRequest,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Pending,
    Reply(Vec<Vec<String>>),
    Reject,
}

// Port of `QuestionPrompt`'s store and handlers.
#[derive(Debug, Clone, Default)]
pub struct QuestionState {
    pub tab: usize,
    pub answers: Vec<Vec<String>>,
    pub custom: Vec<String>,
    pub selected: usize,
    pub editing: Option<Editor>,
}

fn slot<T: Default + Clone>(list: &mut Vec<T>, index: usize) -> &mut T {
    if list.len() <= index {
        list.resize(index + 1, T::default());
    }
    &mut list[index]
}

impl QuestionState {
    pub fn single(questions: &[QuestionInfo]) -> bool {
        questions.len() == 1 && questions[0].multiple != Some(true)
    }

    // Questions plus the confirm tab; a lone single-select question has neither.
    pub fn tabs(questions: &[QuestionInfo]) -> usize {
        if Self::single(questions) { 1 } else { questions.len() + 1 }
    }

    pub fn confirm(&self, questions: &[QuestionInfo]) -> bool {
        !Self::single(questions) && self.tab == questions.len()
    }

    pub fn multi(&self, questions: &[QuestionInfo]) -> bool {
        questions.get(self.tab).is_some_and(|q| q.multiple == Some(true))
    }

    pub fn custom_allowed(&self, questions: &[QuestionInfo]) -> bool {
        questions.get(self.tab).is_some_and(|q| q.custom != Some(false))
    }

    pub fn input(&self) -> &str {
        self.custom.get(self.tab).map_or("", String::as_str)
    }

    pub fn picked(&self, label: &str) -> bool {
        self.answers.get(self.tab).is_some_and(|a| a.iter().any(|x| x == label))
    }

    pub fn custom_picked(&self) -> bool {
        !self.input().is_empty() && self.picked(self.input())
    }

    fn total(&self, questions: &[QuestionInfo]) -> usize {
        questions.get(self.tab).map_or(0, |q| q.options.len()) + usize::from(self.custom_allowed(questions))
    }

    fn other(&self, questions: &[QuestionInfo]) -> bool {
        self.custom_allowed(questions) && questions.get(self.tab).is_some_and(|q| self.selected == q.options.len())
    }

    fn select_tab(&mut self, index: usize) {
        self.tab = index;
        self.selected = 0;
    }

    fn pick(&mut self, questions: &[QuestionInfo], answer: String, custom: bool) -> Outcome {
        *slot(&mut self.answers, self.tab) = vec![answer.clone()];
        if custom {
            *slot(&mut self.custom, self.tab) = answer.clone();
        }
        if Self::single(questions) {
            return Outcome::Reply(vec![vec![answer]]);
        }
        self.select_tab(self.tab + 1);
        Outcome::Pending
    }

    fn toggle(&mut self, answer: &str) {
        let list = slot(&mut self.answers, self.tab);
        match list.iter().position(|x| x == answer) {
            Some(index) => {
                list.remove(index);
            }
            None => list.push(answer.to_string()),
        }
    }

    fn edit(&mut self) {
        self.editing = Some(Editor::with_text(self.input()));
    }

    pub fn select_option(&mut self, questions: &[QuestionInfo]) -> Outcome {
        let multi = self.multi(questions);
        if self.other(questions) {
            if multi && self.custom_picked() {
                let value = self.input().to_string();
                self.toggle(&value);
            } else {
                self.edit();
            }
            return Outcome::Pending;
        }
        let Some(label) = questions.get(self.tab).and_then(|q| q.options.get(self.selected)).map(|o| o.label.clone()) else { return Outcome::Pending };
        if multi {
            self.toggle(&label);
            return Outcome::Pending;
        }
        self.pick(questions, label, false)
    }

    fn submit(&self, questions: &[QuestionInfo]) -> Outcome {
        Outcome::Reply((0..questions.len()).map(|i| self.answers.get(i).cloned().unwrap_or_default()).collect())
    }

    // Return pressed while typing a custom answer.
    pub fn submit_edit(&mut self, questions: &[QuestionInfo]) -> Outcome {
        let Some(editor) = self.editing.take() else { return Outcome::Pending };
        let text = editor.text.trim().to_string();
        let previous = self.input().to_string();
        if text.is_empty() {
            if !previous.is_empty() {
                *slot(&mut self.custom, self.tab) = String::new();
                slot(&mut self.answers, self.tab).retain(|x| *x != previous);
            }
            return Outcome::Pending;
        }
        if self.multi(questions) {
            *slot(&mut self.custom, self.tab) = text.clone();
            let list = slot(&mut self.answers, self.tab);
            list.retain(|x| previous.is_empty() || *x != previous);
            if !list.contains(&text) {
                list.push(text);
            }
            return Outcome::Pending;
        }
        self.pick(questions, text, true)
    }

    // `prompt.clear` while editing: empties the field, or leaves edit mode when it is already empty.
    pub fn clear_edit(&mut self) {
        match &mut self.editing {
            Some(editor) if !editor.is_empty() => editor.clear(),
            _ => self.editing = None,
        }
    }

    // Keys outside the custom-answer field.
    pub fn key(&mut self, request: &QuestionRequest, stroke: &KeyStroke) -> Outcome {
        let questions = request.questions.as_slice();
        if stroke.ctrl || stroke.alt {
            return Outcome::Pending;
        }
        let tabs = Self::tabs(questions);
        let total = self.total(questions);
        let confirm = self.confirm(questions);
        match stroke.name.as_str() {
            "left" | "h" => self.select_tab((self.tab + tabs - 1) % tabs),
            "right" | "l" => self.select_tab((self.tab + 1) % tabs),
            "tab" => self.select_tab((self.tab + if stroke.shift { tabs - 1 } else { 1 }) % tabs),
            "escape" => return Outcome::Reject,
            "return" if confirm => return self.submit(questions),
            _ if confirm || total == 0 => {}
            "up" | "k" => self.selected = (self.selected + total - 1) % total,
            "down" | "j" => self.selected = (self.selected + 1) % total,
            "return" => return self.select_option(questions),
            name => {
                let digit = name.chars().next().filter(|_| name.len() == 1).and_then(|c| c.to_digit(10)).map(|d| d as usize);
                if let Some(index) = digit.filter(|d| (1..=total.min(9)).contains(d)) {
                    self.selected = index - 1;
                    return self.select_option(questions);
                }
            }
        }
        Outcome::Pending
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use opencode_session::QuestionOption;

    pub fn info(header: &str, options: &[&str], multiple: bool) -> QuestionInfo {
        QuestionInfo {
            question: format!("{header}?"),
            header: header.into(),
            options: options.iter().map(|label| QuestionOption { label: label.to_string(), description: format!("about {label}") }).collect(),
            multiple: multiple.then_some(true),
            custom: None,
        }
    }

    pub fn request(questions: Vec<QuestionInfo>) -> QuestionRequest {
        QuestionRequest { id: "que_1".into(), session_id: "ses_1".into(), questions, tool: None }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    fn stroke(name: &str) -> KeyStroke {
        KeyStroke { name: name.into(), ..Default::default() }
    }

    #[test]
    fn single_select_replies_at_once() {
        let request = request(vec![info("Color", &["Red", "Blue"], false)]);
        let mut state = QuestionState::default();
        assert_eq!(state.key(&request, &stroke("down")), Outcome::Pending);
        assert_eq!(state.key(&request, &stroke("return")), Outcome::Reply(vec![vec!["Blue".into()]]));
        let mut state = QuestionState::default();
        assert_eq!(state.key(&request, &stroke("1")), Outcome::Reply(vec![vec!["Red".into()]]));
        assert_eq!(QuestionState::default().key(&request, &stroke("escape")), Outcome::Reject);
        // "3" is the custom answer row.
        let mut state = QuestionState::default();
        assert_eq!(state.key(&request, &stroke("3")), Outcome::Pending);
        state.editing.as_mut().unwrap().insert("Green");
        assert_eq!(state.submit_edit(&request.questions), Outcome::Reply(vec![vec!["Green".into()]]));
    }

    #[test]
    fn multi_select_toggles_then_confirms() {
        let request = request(vec![info("Color", &["Red", "Blue"], true), info("Size", &["S", "L"], false)]);
        let mut state = QuestionState::default();
        state.key(&request, &stroke("return"));
        state.key(&request, &stroke("j"));
        state.key(&request, &stroke("return"));
        state.key(&request, &stroke("k"));
        state.key(&request, &stroke("return"));
        assert_eq!(state.answers[0], vec!["Blue".to_string()]);
        // Custom answer in a multi-select question is added, then toggled off again.
        state.key(&request, &stroke("3"));
        state.editing.as_mut().unwrap().insert("Teal");
        assert_eq!(state.submit_edit(&request.questions), Outcome::Pending);
        assert_eq!(state.answers[0], vec!["Blue".to_string(), "Teal".to_string()]);
        assert!(state.custom_picked());
        state.key(&request, &stroke("return"));
        assert_eq!(state.answers[0], vec!["Blue".to_string()]);
        state.key(&request, &stroke("tab"));
        assert_eq!(state.tab, 1);
        assert_eq!(state.key(&request, &stroke("2")), Outcome::Pending);
        assert!(state.confirm(&request.questions));
        assert_eq!(state.key(&request, &stroke("return")), Outcome::Reply(vec![vec!["Blue".into()], vec!["L".into()]]));
        state.key(&request, &KeyStroke { name: "tab".into(), shift: true, ..Default::default() });
        assert_eq!(state.tab, 1);
    }

    #[test]
    fn clearing_and_emptying_a_custom_answer() {
        let request = request(vec![info("Color", &["Red"], true)]);
        let mut state = QuestionState::default();
        state.key(&request, &stroke("2"));
        state.editing.as_mut().unwrap().insert("Teal");
        state.clear_edit();
        assert!(state.editing.as_ref().unwrap().is_empty());
        state.clear_edit();
        assert!(state.editing.is_none());
        state.key(&request, &stroke("return"));
        state.editing.as_mut().unwrap().insert("Teal");
        state.submit_edit(&request.questions);
        state.key(&request, &stroke("return"));
        state.key(&request, &stroke("return"));
        state.editing.as_mut().unwrap().clear();
        state.submit_edit(&request.questions);
        assert_eq!((state.input(), state.answers[0].len()), ("", 0));
        let no_custom = super::testing::request(vec![QuestionInfo { custom: Some(false), ..info("Color", &["Red"], true) }]);
        let mut state = QuestionState::default();
        state.key(&no_custom, &stroke("2"));
        assert!(state.editing.is_none());
    }
}
