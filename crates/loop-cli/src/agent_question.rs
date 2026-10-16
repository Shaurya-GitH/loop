//! The TUI setup box for what an agent asks the user: a question, a form, or
//! a link to open (ACP elicitation, or any backend's [`InteractionRequest`]).
//!
//! A form is asked one field at a time. The answer goes back through the
//! request's reply channel, which lives in the wizard: closing it without an
//! answer (esc, or the request being withdrawn) cancels the request.

use std::sync::{Arc, Mutex};

use loop_protocol::{InteractionId, InteractionOutcome, InteractionRequest, DECLINE_CHOICE};
use serde_json::{Map, Value};
use tokio::sync::oneshot;

use crate::wizard::{wrap_selection, Transition, WizardView};

/// Choice id for opening a link.
pub const OPEN_CHOICE: &str = "open";

/// What the agent asks, and the answers so far.
#[derive(Debug, Clone)]
pub struct AgentQuestion {
    id: InteractionId,
    title: String,
    kind: Kind,
    selected: usize,
    reply: Arc<Mutex<Option<oneshot::Sender<InteractionOutcome>>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// A form, asked field by field.
    Form {
        fields: Vec<Field>,
        index: usize,
        answers: Vec<(String, String)>,
    },
    /// A link the user is asked to open.
    Url { url: String },
    /// A question with offered answers, or free text when `choices` is empty.
    Question { choices: Vec<(String, String)> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    name: String,
    title: String,
    description: Option<String>,
    required: bool,
    default: Option<String>,
    input: Input,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Input {
    Text,
    Integer,
    Number,
    Bool,
    /// `(value, label)` pairs.
    Choice(Vec<(String, String)>),
    /// Comma-separated values.
    List,
}

impl PartialEq for AgentQuestion {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.kind == other.kind
            && self.selected == other.selected
            && Arc::ptr_eq(&self.reply, &other.reply)
    }
}

impl Eq for AgentQuestion {}

impl AgentQuestion {
    /// The wizard for `request`, or `None` for kinds it does not present
    /// (permission requests have their own picker).
    pub fn new(
        request: InteractionRequest,
        reply: oneshot::Sender<InteractionOutcome>,
    ) -> Result<Self, oneshot::Sender<InteractionOutcome>> {
        let (id, title, kind) = match request {
            InteractionRequest::Form { id, title, schema } => {
                let fields = form_fields(&schema);
                let title = title
                    .or_else(|| {
                        schema
                            .get("title")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "The agent needs some details".into());
                (
                    id,
                    title,
                    Kind::Form {
                        fields,
                        index: 0,
                        answers: Vec::new(),
                    },
                )
            }
            InteractionRequest::Url { id, url, purpose } => {
                let title = if purpose.is_empty() {
                    "The agent asks you to open a link".into()
                } else {
                    purpose
                };
                (id, title, Kind::Url { url })
            }
            InteractionRequest::Question {
                id,
                prompt,
                choices,
            } => {
                let choices = choices
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| (c.native_id, c.label))
                    .collect();
                (id, prompt, Kind::Question { choices })
            }
            _ => return Err(reply),
        };
        Ok(Self {
            id,
            title,
            kind,
            selected: 0,
            reply: Arc::new(Mutex::new(Some(reply))),
        })
    }

    /// The interaction this answers.
    pub fn id(&self) -> &InteractionId {
        &self.id
    }

    /// Feed the input line (or the highlighted choice) to the current step.
    pub fn submit(mut self, value: &str) -> Transition<Self, ()> {
        let selected = self.selected;
        match &mut self.kind {
            Kind::Url { url } => {
                let open = selected == 0;
                if open {
                    let _ = crate::app::open_link(url);
                }
                let native_id = if open { OPEN_CHOICE } else { DECLINE_CHOICE };
                self.answer(InteractionOutcome::Selected {
                    native_id: native_id.into(),
                    note: None,
                });
                Transition::Done(())
            }
            Kind::Question { choices } if !choices.is_empty() => {
                let native_id = choices[selected.min(choices.len() - 1)].0.clone();
                self.answer(InteractionOutcome::Selected {
                    native_id,
                    note: None,
                });
                Transition::Done(())
            }
            Kind::Question { .. } => {
                if value.is_empty() {
                    return Transition::Retry {
                        step: self,
                        error: "type an answer, or esc to dismiss".into(),
                    };
                }
                self.answer(InteractionOutcome::Answered {
                    value: Value::String(value.to_owned()),
                });
                Transition::Done(())
            }
            Kind::Form { .. } => self.submit_field(value, selected),
        }
    }

    /// One form field's answer: record it, then ask the next field or finish.
    fn submit_field(mut self, value: &str, selected: usize) -> Transition<Self, ()> {
        let Kind::Form {
            fields,
            index,
            answers,
        } = &mut self.kind
        else {
            return Transition::Done(());
        };
        let Some(field) = fields.get(*index).cloned() else {
            let content = form_content(fields, answers);
            self.answer(InteractionOutcome::Answered { value: content });
            return Transition::Done(());
        };
        let raw = match &field.input {
            Input::Bool => if selected == 0 { "true" } else { "false" }.to_owned(),
            Input::Choice(options) => options[selected.min(options.len() - 1)].0.clone(),
            _ => value.to_owned(),
        };
        if raw.is_empty() && field.required {
            return Transition::Retry {
                step: self,
                error: format!("{} is required", field.title),
            };
        }
        if !raw.is_empty() {
            if let Err(error) = check(&field.input, &raw) {
                return Transition::Retry { step: self, error };
            }
            answers.push((field.name.clone(), raw));
        }
        *index += 1;
        let next = fields
            .get(*index)
            .map(|f| f.default.clone().unwrap_or_default());
        self.selected = 0;
        match next {
            Some(prefill) => Transition::Next {
                step: self,
                prefill,
            },
            None => {
                let content = match &self.kind {
                    Kind::Form {
                        fields, answers, ..
                    } => form_content(fields, answers),
                    _ => Value::Null,
                };
                self.answer(InteractionOutcome::Answered { value: content });
                Transition::Done(())
            }
        }
    }

    fn answer(&self, outcome: InteractionOutcome) {
        if let Some(reply) = self.reply.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = reply.send(outcome);
        }
    }

    fn field(&self) -> Option<&Field> {
        match &self.kind {
            Kind::Form { fields, index, .. } => fields.get(*index),
            _ => None,
        }
    }
}

impl WizardView for AgentQuestion {
    fn title(&self) -> String {
        self.title.clone()
    }

    fn instructions(&self) -> String {
        match &self.kind {
            Kind::Url { url } => url.clone(),
            Kind::Question { choices } if !choices.is_empty() => "Choose an answer".into(),
            Kind::Question { .. } => "Type your answer".into(),
            Kind::Form { fields, index, .. } => {
                let Some(field) = fields.get(*index) else {
                    return String::new();
                };
                let mut text = field.title.clone();
                if fields.len() > 1 {
                    text = format!("{text} ({} of {})", index + 1, fields.len());
                }
                if let Some(description) = &field.description {
                    text = format!("{text} — {description}");
                }
                if !field.required {
                    text.push_str(" · optional, enter to skip");
                }
                text
            }
        }
    }

    fn placeholder(&self) -> &'static str {
        match self.field().map(|f| &f.input) {
            Some(Input::Integer | Input::Number) => "a number",
            Some(Input::List) => "values, separated by commas",
            _ => "your answer",
        }
    }

    fn options(&self) -> Option<(Vec<(String, String)>, usize)> {
        let rows = match &self.kind {
            Kind::Url { .. } => vec![
                ("Open link".into(), "in your browser".into()),
                ("Decline".into(), String::new()),
            ],
            Kind::Question { choices } if !choices.is_empty() => choices
                .iter()
                .map(|(_, label)| (label.clone(), String::new()))
                .collect(),
            Kind::Form { .. } => match self.field().map(|f| &f.input) {
                Some(Input::Bool) => {
                    vec![("Yes".into(), String::new()), ("No".into(), String::new())]
                }
                Some(Input::Choice(options)) => options
                    .iter()
                    .map(|(_, label)| (label.clone(), String::new()))
                    .collect(),
                _ => return None,
            },
            Kind::Question { .. } => return None,
        };
        Some((rows, self.selected))
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.options().map_or(0, |(rows, _)| rows.len());
        self.selected = wrap_selection(self.selected, delta, len);
    }
}

/// The fields of an elicitation form schema (flat object, primitive
/// properties), in the schema's order.
fn form_fields(schema: &Value) -> Vec<Field> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    properties
        .iter()
        .map(|(name, property)| {
            let text = |key: &str| property.get(key).and_then(Value::as_str).map(str::to_owned);
            let input = match property.get("type").and_then(Value::as_str) {
                Some("boolean") => Input::Bool,
                Some("integer") => Input::Integer,
                Some("number") => Input::Number,
                Some("array") => Input::List,
                _ => choice_options(property).map_or(Input::Text, Input::Choice),
            };
            let default = property.get("default").and_then(|d| match d {
                Value::String(s) => Some(s.clone()),
                Value::Null => None,
                other => Some(other.to_string()),
            });
            Field {
                name: name.clone(),
                title: text("title").unwrap_or_else(|| name.clone()),
                description: text("description"),
                required: required.contains(&name.as_str()),
                default,
                input,
            }
        })
        .collect()
}

/// `(value, label)` options of a string property with `oneOf` or `enum`.
fn choice_options(property: &Value) -> Option<Vec<(String, String)>> {
    if let Some(one_of) = property.get("oneOf").and_then(Value::as_array) {
        let options: Vec<_> = one_of
            .iter()
            .filter_map(|o| {
                let value = o.get("const").and_then(Value::as_str)?;
                let label = o.get("title").and_then(Value::as_str).unwrap_or(value);
                Some((value.to_owned(), label.to_owned()))
            })
            .collect();
        return (!options.is_empty()).then_some(options);
    }
    let values = property.get("enum").and_then(Value::as_array)?;
    let names = property.get("enumNames").and_then(Value::as_array);
    let options: Vec<_> = values
        .iter()
        .enumerate()
        .filter_map(|(i, v)| {
            let value = v.as_str()?;
            let label = names
                .and_then(|n| n.get(i))
                .and_then(Value::as_str)
                .unwrap_or(value);
            Some((value.to_owned(), label.to_owned()))
        })
        .collect();
    (!options.is_empty()).then_some(options)
}

fn check(input: &Input, raw: &str) -> Result<(), String> {
    match input {
        Input::Integer if raw.parse::<i64>().is_err() => Err("enter a whole number".into()),
        Input::Number if raw.parse::<f64>().is_err() => Err("enter a number".into()),
        _ => Ok(()),
    }
}

/// The answers as the JSON object the form asked for.
fn form_content(fields: &[Field], answers: &[(String, String)]) -> Value {
    let mut content = Map::new();
    for (name, raw) in answers {
        let Some(field) = fields.iter().find(|f| &f.name == name) else {
            continue;
        };
        let value = match field.input {
            Input::Bool => Value::Bool(raw == "true"),
            Input::Integer => raw.parse::<i64>().map(Value::from).unwrap_or(Value::Null),
            Input::Number => raw.parse::<f64>().map(Value::from).unwrap_or(Value::Null),
            Input::List => Value::Array(
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| Value::String(s.to_owned()))
                    .collect(),
            ),
            _ => Value::String(raw.clone()),
        };
        content.insert(name.clone(), value);
    }
    Value::Object(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn form(schema: Value) -> (AgentQuestion, oneshot::Receiver<InteractionOutcome>) {
        let (tx, rx) = oneshot::channel();
        let request = InteractionRequest::Form {
            id: InteractionId("f".into()),
            title: Some("Details".into()),
            schema,
        };
        (AgentQuestion::new(request, tx).unwrap(), rx)
    }

    fn next(t: Transition<AgentQuestion, ()>) -> AgentQuestion {
        match t {
            Transition::Next { step, .. } | Transition::Retry { step, .. } => step,
            Transition::Done(()) => panic!("not done yet"),
        }
    }

    #[test]
    fn a_form_is_asked_field_by_field_and_answered_as_typed_json() {
        let (q, mut rx) = form(json!({"type": "object",
            "properties": {
                "count": {"type": "integer", "title": "How many"},
                "mode": {"type": "string", "oneOf": [
                    {"const": "fast", "title": "Fast"}, {"const": "safe", "title": "Safe"}]},
                "name": {"type": "string"},
                "ok": {"type": "boolean"}
            },
            "required": ["count", "name"]}));
        assert!(q.instructions().starts_with("How many (1 of 4)"));

        let q = match q.submit("two") {
            Transition::Retry { step, error } => {
                assert_eq!(error, "enter a whole number");
                step
            }
            other => panic!("expected a retry, got {other:?}"),
        };
        let mut q = next(q.submit("2"));
        assert_eq!(q.options().unwrap().0[1].0, "Safe");
        q.move_selection(1);
        let q = next(q.submit(""));
        assert!(
            matches!(q.clone().submit(""), Transition::Retry { .. }),
            "name is required"
        );
        let mut q = next(q.submit("Ada"));
        q.move_selection(1);
        assert!(matches!(q.submit(""), Transition::Done(())));

        assert_eq!(
            rx.try_recv().unwrap(),
            InteractionOutcome::Answered {
                value: json!({"count": 2, "mode": "safe", "name": "Ada", "ok": false})
            }
        );
    }

    #[test]
    fn optional_fields_can_be_skipped() {
        let (q, mut rx) = form(json!({"type": "object",
            "properties": {"note": {"type": "string"}}}));
        assert!(q.instructions().contains("optional"));
        assert!(matches!(q.submit(""), Transition::Done(())));
        assert_eq!(
            rx.try_recv().unwrap(),
            InteractionOutcome::Answered { value: json!({}) }
        );
    }

    #[test]
    fn closing_without_an_answer_cancels() {
        let (q, mut rx) = form(json!({"type": "object", "properties": {}}));
        drop(q);
        assert!(
            rx.try_recv().is_err(),
            "the sender was dropped, so the request is cancelled"
        );
    }

    #[test]
    fn a_link_can_be_declined() {
        let (tx, mut rx) = oneshot::channel();
        let request = InteractionRequest::Url {
            id: InteractionId("u".into()),
            url: "https://example.com".into(),
            purpose: "Sign in".into(),
        };
        let mut q = AgentQuestion::new(request, tx).unwrap();
        assert_eq!(q.title(), "Sign in");
        q.move_selection(1);
        assert!(matches!(q.submit(""), Transition::Done(())));
        assert_eq!(
            rx.try_recv().unwrap(),
            InteractionOutcome::Selected {
                native_id: DECLINE_CHOICE.into(),
                note: None
            }
        );
    }

    #[test]
    fn questions_with_choices_answer_with_the_choice_id() {
        let (tx, mut rx) = oneshot::channel();
        let request = InteractionRequest::Question {
            id: InteractionId("q".into()),
            prompt: "Proceed?".into(),
            choices: Some(vec![
                loop_protocol::Choice {
                    native_id: "y".into(),
                    label: "Yes".into(),
                },
                loop_protocol::Choice {
                    native_id: "n".into(),
                    label: "No".into(),
                },
            ]),
        };
        let mut q = AgentQuestion::new(request, tx).unwrap();
        q.move_selection(1);
        assert!(matches!(q.submit(""), Transition::Done(())));
        assert_eq!(
            rx.try_recv().unwrap(),
            InteractionOutcome::Selected {
                native_id: "n".into(),
                note: None
            }
        );
    }
}
