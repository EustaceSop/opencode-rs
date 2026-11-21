// A pending `question` tool call and the answers being put together for it.
use opencode_session::QuestionInfo;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct QuestionView {
    pub id: u64,
    pub session_id: Arc<str>,
    pub questions: Vec<QuestionInfo>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuestionForm {
    pub id: u64,
    selected: Vec<Vec<String>>,
    custom: Vec<String>,
}

fn multiple(question: &QuestionInfo) -> bool {
    question.multiple == Some(true)
}

// The original offers a free-text answer unless the question turns it off.
pub fn allows_custom(question: &QuestionInfo) -> bool {
    question.custom != Some(false)
}

impl QuestionForm {
    pub fn new(view: &QuestionView) -> Self {
        let count = view.questions.len();
        Self { id: view.id, selected: vec![Vec::new(); count], custom: vec![String::new(); count] }
    }

    pub fn is_selected(&self, question: usize, label: &str) -> bool {
        self.selected.get(question).is_some_and(|labels| labels.iter().any(|l| l == label))
    }

    pub fn toggle(&mut self, view: &QuestionView, question: usize, label: &str) {
        let (Some(info), Some(labels)) = (view.questions.get(question), self.selected.get_mut(question)) else {
            return;
        };
        if !info.options.iter().any(|o| o.label == label) {
            return;
        }
        let chosen = labels.iter().position(|l| l == label);
        match (multiple(info), chosen) {
            (true, Some(index)) => drop(labels.remove(index)),
            (true, None) => labels.push(label.to_string()),
            (false, Some(_)) => labels.clear(),
            (false, None) => *labels = vec![label.to_string()],
        }
    }

    pub fn set_custom(&mut self, question: usize, text: &str) {
        if let Some(slot) = self.custom.get_mut(question) {
            *slot = text.to_string();
        }
    }

    // Selected labels in option order, then the typed answer; a typed answer replaces a single choice.
    pub fn answers(&self, view: &QuestionView) -> Vec<Vec<String>> {
        view.questions
            .iter()
            .enumerate()
            .map(|(index, info)| {
                let typed = self.custom.get(index).map(|t| t.trim()).filter(|t| !t.is_empty() && allows_custom(info));
                let chosen = info.options.iter().filter(|o| self.is_selected(index, &o.label)).map(|o| o.label.clone());
                match (multiple(info), typed) {
                    (false, Some(text)) => vec![text.to_string()],
                    (_, typed) => chosen.chain(typed.map(str::to_string)).collect(),
                }
            })
            .collect()
    }

    pub fn answered(&self, view: &QuestionView) -> usize {
        self.answers(view).iter().filter(|a| !a.is_empty()).count()
    }

    pub fn complete(&self, view: &QuestionView) -> bool {
        self.answered(view) == view.questions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencode_session::QuestionOption;

    fn question(header: &str, options: &[&str], multiple: Option<bool>, custom: Option<bool>) -> QuestionInfo {
        QuestionInfo {
            question: format!("{header}?"),
            header: header.into(),
            options: options.iter().map(|label| QuestionOption { label: label.to_string(), description: String::new() }).collect(),
            multiple,
            custom,
        }
    }

    fn view() -> QuestionView {
        QuestionView {
            id: 3,
            session_id: "ses_1".into(),
            questions: vec![
                question("Color", &["Red", "Green"], None, None),
                question("Tools", &["Bash", "Edit", "Read"], Some(true), None),
                question("Sure", &["Yes", "No"], None, Some(false)),
            ],
        }
    }

    #[test]
    fn single_choice_replaces_and_multiple_accumulates_in_option_order() {
        let view = view();
        let mut form = QuestionForm::new(&view);
        assert_eq!(form.answered(&view), 0);
        form.toggle(&view, 0, "Red");
        form.toggle(&view, 0, "Green");
        form.toggle(&view, 1, "Read");
        form.toggle(&view, 1, "Bash");
        form.toggle(&view, 1, "Edit");
        form.toggle(&view, 1, "Edit");
        form.toggle(&view, 1, "Unknown");
        form.toggle(&view, 9, "Red");
        assert!(form.is_selected(0, "Green") && !form.is_selected(0, "Red"));
        assert!(!form.complete(&view));
        form.toggle(&view, 2, "Yes");
        assert_eq!(form.answers(&view), [vec!["Green".to_string()], vec!["Bash".into(), "Read".into()], vec!["Yes".into()]]);
        assert!(form.complete(&view));

        form.toggle(&view, 0, "Green");
        assert_eq!(form.answered(&view), 2);
    }

    #[test]
    fn typed_answers_replace_a_single_choice_and_extend_a_multiple_one() {
        let view = view();
        let mut form = QuestionForm::new(&view);
        form.toggle(&view, 0, "Red");
        form.set_custom(0, "  Blue ");
        form.toggle(&view, 1, "Edit");
        form.set_custom(1, "Grep");
        form.set_custom(2, "maybe");
        form.set_custom(7, "ignored");
        assert_eq!(form.answers(&view), [vec!["Blue".to_string()], vec!["Edit".into(), "Grep".into()], vec![]]);
        assert!(allows_custom(&view.questions[0]) && !allows_custom(&view.questions[2]));
    }
}
