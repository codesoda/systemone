//! Jev wire format ⇄ neutral types.
//!
//! Parsing is strict about *structure* (duplicate keys, unknown fields,
//! depth, types) and permissive about *values*: floats, nulls and empty
//! entries reach the adapter untouched so each host can apply its own
//! documented rules. Rendering rounds every probability to two decimals
//! without renormalizing, matching the reference SDK fixtures.

use std::collections::HashMap;

use serde::{
    Deserialize,
    de::{self, Deserializer, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};
use systemone_core::{
    Answer, ChoiceQuestion, DecisionRequest, DecisionResponse, HostError, NoulQuestion, Question,
    ScoreQuestion,
};

/// Maximum nesting depth accepted on the wire.
pub const MAX_JSON_DEPTH: usize = 64;

/// Maximum length of an upstream `id` kept as the provider request id. A
/// longer id is dropped, because it travels in a response header.
pub const MAX_PROVIDER_REQUEST_ID_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireError {
    /// `invalid_json` or `validation_error`.
    pub error_type: &'static str,
    pub message: String,
}

impl WireError {
    fn bad_json(message: impl Into<String>) -> Self {
        Self {
            error_type: "invalid_json",
            message: message.into(),
        }
    }

    fn validation(message: impl Into<String>) -> Self {
        Self {
            error_type: "validation_error",
            message: message.into(),
        }
    }
}

impl From<HostError> for WireError {
    fn from(error: HostError) -> Self {
        Self::validation(error.to_string())
    }
}

/// A parsed request plus the SystemOne routing extension.
#[derive(Debug, PartialEq)]
pub struct ParsedRequest {
    pub request: DecisionRequest,
    /// Top-level `backend` field, if present.
    pub backend: Option<String>,
}

pub fn parse_request(bytes: &[u8]) -> Result<ParsedRequest, WireError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| WireError::bad_json("request body must be valid UTF-8 JSON"))?;
    let value = parse_strict(text)?;
    let mut request = into_object(value, "request")?;
    let model = match request.shift_remove("model") {
        None => None,
        Some(Value::String(model)) if !model.is_empty() => Some(model),
        Some(_) => {
            return Err(WireError::validation(
                "model must be a nonempty string when provided",
            ));
        }
    };
    let backend = match request.shift_remove("backend") {
        None => None,
        Some(Value::String(backend)) if !backend.is_empty() => Some(backend),
        Some(_) => {
            return Err(WireError::validation(
                "backend must be a nonempty string when provided",
            ));
        }
    };
    let state = request
        .shift_remove("state")
        .ok_or_else(|| WireError::validation("state is required"))?;
    require_entry_type(&state, "state")?;
    let questions = request
        .shift_remove("questions")
        .ok_or_else(|| WireError::validation("questions is required"))?;
    reject_unknown(&request, "request")?;
    let questions = into_object(questions, "questions")?;
    if questions.is_empty() {
        return Err(WireError::validation(
            "questions must contain at least one question",
        ));
    }
    let mut parsed = Vec::with_capacity(questions.len());
    for (external_id, question) in questions {
        let question = parse_question(&external_id, question)?;
        parsed.push((external_id, question));
    }
    let request = DecisionRequest::new(model, state, parsed)?;
    Ok(ParsedRequest { request, backend })
}

fn parse_question(external_id: &str, value: Value) -> Result<Question, WireError> {
    let path = format!("questions.{external_id:?}");
    let mut question = into_object(value, &path)?;
    let kind = match question.shift_remove("type") {
        Some(Value::String(kind)) => kind,
        Some(_) => {
            return Err(WireError::validation(format!(
                "{path}.type must be a string"
            )));
        }
        None => return Err(WireError::validation(format!("{path}.type is required"))),
    };
    let instructions = question.shift_remove("instructions");
    if let Some(instructions) = &instructions {
        require_entry_type(instructions, &format!("{path}.instructions"))?;
    }
    let criteria = question.shift_remove("criteria");
    reject_unknown(&question, &path)?;
    match kind.as_str() {
        "choice" => {
            let criteria = criteria.ok_or_else(|| {
                WireError::validation(format!("{path}.criteria is required for choice"))
            })?;
            let criteria = into_object(criteria, &format!("{path}.criteria"))?;
            if criteria.is_empty() {
                return Err(WireError::validation(format!(
                    "{path}.criteria must contain at least one option"
                )));
            }
            let mut pairs = Vec::with_capacity(criteria.len());
            for (label, description) in criteria {
                require_entry_type(&description, &format!("{path}.criteria.{label:?}"))?;
                pairs.push((label, description));
            }
            Ok(Question::Choice(ChoiceQuestion {
                instructions,
                criteria: pairs,
            }))
        }
        "noul" => {
            let mut true_description = None;
            let mut false_description = None;
            if let Some(criteria) = criteria
                && !criteria.is_null()
            {
                let mut criteria = into_object(criteria, &format!("{path}.criteria"))?;
                if let Some(value) = criteria.shift_remove("true") {
                    require_entry_type(&value, &format!("{path}.criteria.true"))?;
                    true_description = Some(value);
                }
                if let Some(value) = criteria.shift_remove("false") {
                    require_entry_type(&value, &format!("{path}.criteria.false"))?;
                    false_description = Some(value);
                }
                reject_unknown(&criteria, &format!("{path}.criteria"))?;
            }
            Ok(Question::Noul(NoulQuestion {
                instructions,
                true_description,
                false_description,
            }))
        }
        "score" => {
            let levels = match criteria {
                Some(Value::Array(levels)) => levels,
                Some(_) => {
                    return Err(WireError::validation(format!(
                        "{path}.criteria must be an array for score"
                    )));
                }
                None => {
                    return Err(WireError::validation(format!(
                        "{path}.criteria is required for score"
                    )));
                }
            };
            if levels.len() < 2 {
                return Err(WireError::validation(format!(
                    "{path}.criteria must contain at least two levels"
                )));
            }
            for (index, level) in levels.iter().enumerate() {
                require_entry_type(level, &format!("{path}.criteria[{index}]"))?;
            }
            Ok(Question::Score(ScoreQuestion {
                instructions,
                levels,
            }))
        }
        _ => Err(WireError::validation(format!(
            "{path}.type must be choice, noul, or score"
        ))),
    }
}

fn require_entry_type(value: &Value, path: &str) -> Result<(), WireError> {
    if matches!(
        value,
        Value::String(_) | Value::Object(_) | Value::Array(_) | Value::Null
    ) {
        Ok(())
    } else {
        Err(WireError::validation(format!(
            "{path} must be a string, object, array, or null"
        )))
    }
}

fn into_object(value: Value, path: &str) -> Result<Map<String, Value>, WireError> {
    match value {
        Value::Object(value) => Ok(value),
        _ => Err(WireError::validation(format!(
            "{path} must be a JSON object"
        ))),
    }
}

fn reject_unknown(fields: &Map<String, Value>, path: &str) -> Result<(), WireError> {
    if let Some(field) = fields.keys().next() {
        Err(WireError::validation(format!(
            "unsupported field {field:?} in {path}"
        )))
    } else {
        Ok(())
    }
}

/// Parse JSON preserving object order, rejecting duplicate keys and nesting
/// deeper than [`MAX_JSON_DEPTH`].
pub fn parse_strict(text: &str) -> Result<Value, WireError> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = deserializer
        .deserialize_any(StrictVisitor { depth: 0 })
        .map_err(classify)?;
    deserializer
        .end()
        .map_err(|error| WireError::bad_json(format!("trailing characters: {error}")))?;
    Ok(value)
}

fn classify(error: serde_json::Error) -> WireError {
    let message = error.to_string();
    if message.starts_with("duplicate JSON key") || message.starts_with("JSON nesting exceeds") {
        WireError::validation(message)
    } else {
        WireError::bad_json(message)
    }
}

struct StrictVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite number"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(de::Error::custom(format!(
                "JSON nesting exceeds maximum depth {MAX_JSON_DEPTH}"
            )));
        }
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(StrictSeed {
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(de::Error::custom(format!(
                "JSON nesting exceeds maximum depth {MAX_JSON_DEPTH}"
            )));
        }
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value_seed(StrictSeed {
                depth: self.depth + 1,
            })?;
            if object.insert(key.clone(), value).is_some() {
                return Err(de::Error::custom(format!("duplicate JSON key {key:?}")));
            }
        }
        Ok(Value::Object(object))
    }
}

struct StrictSeed {
    depth: usize,
}

impl<'de> de::DeserializeSeed<'de> for StrictSeed {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(StrictVisitor { depth: self.depth })
    }
}

/// Body shape returned by `POST /v1/systemone`.
#[derive(Debug, Deserialize, PartialEq, serde::Serialize)]
pub struct SystemOneBody {
    pub model: String,
    pub answers: Map<String, Value>,
    pub usage: Map<String, Value>,
}

/// Round `value` to two decimals for the wire without renormalizing.
#[must_use]
pub fn round_wire(value: f64) -> f64 {
    let rounded = (value * 100.0).round() / 100.0;
    if rounded == 0.0 { 0.0 } else { rounded }
}

#[must_use]
pub fn render_response(response: &DecisionResponse) -> SystemOneBody {
    let mut answers = Map::new();
    for (id, answer) in &response.answers {
        answers.insert(id.clone(), render_answer(answer));
    }
    let mut usage = Map::new();
    // Unknown usage is omitted rather than reported as zero.
    if let Some(input) = response.usage.input_tokens {
        usage.insert("input_tokens".to_owned(), Value::from(input));
    }
    if let Some(output) = response.usage.output_tokens {
        usage.insert("output_tokens".to_owned(), Value::from(output));
    }
    if let Some(cost) = response.usage.cost {
        usage.insert("cost".to_owned(), Value::from(cost));
    }
    SystemOneBody {
        model: response.model.clone(),
        answers,
        usage,
    }
}

fn render_answer(answer: &Answer) -> Value {
    let mut object = Map::new();
    match answer {
        Answer::Choice(choice) => {
            object.insert("type".to_owned(), Value::from("choice"));
            object.insert("choice".to_owned(), Value::from(choice.choice.clone()));
            if let Some(confidence) = choice.confidence {
                object.insert("confidence".to_owned(), Value::from(round_wire(confidence)));
            }
            let mut probabilities = Map::new();
            for (label, probability) in &choice.probabilities {
                probabilities.insert(label.clone(), Value::from(round_wire(*probability)));
            }
            object.insert("probabilities".to_owned(), Value::Object(probabilities));
        }
        Answer::Noul(noul) => {
            object.insert("type".to_owned(), Value::from("noul"));
            object.insert(
                "noul".to_owned(),
                Value::from(round_wire(noul.probability_true)),
            );
        }
        Answer::Score(score) => {
            object.insert("type".to_owned(), Value::from("score"));
            object.insert("score".to_owned(), Value::from(round_wire(score.score)));
            if let Some(confidence) = score.confidence {
                object.insert("confidence".to_owned(), Value::from(round_wire(confidence)));
            }
            let mut legend = Map::new();
            let mut probabilities = Map::new();
            for (index, (description, probability)) in
                score.legend.iter().zip(&score.probabilities).enumerate()
            {
                legend.insert(index.to_string(), description.clone());
                probabilities.insert(index.to_string(), Value::from(round_wire(*probability)));
            }
            object.insert("legend".to_owned(), Value::Object(legend));
            object.insert("probabilities".to_owned(), Value::Object(probabilities));
        }
    }
    Value::Object(object)
}

/// Render a neutral request as the Jev wire body used to call a
/// Jev-compatible hosted API. `model` is the resolved model id; the
/// SystemOne routing selectors are never part of the rendered body.
#[must_use]
pub fn render_request(request: &DecisionRequest, model: &str) -> Value {
    let mut object = Map::new();
    object.insert("model".to_owned(), Value::from(model));
    object.insert("state".to_owned(), request.state.clone());
    let mut questions = Map::new();
    for (id, question) in &request.questions {
        questions.insert(id.clone(), render_question(question));
    }
    object.insert("questions".to_owned(), Value::Object(questions));
    Value::Object(object)
}

fn render_question(question: &Question) -> Value {
    let mut object = Map::new();
    match question {
        Question::Choice(choice) => {
            object.insert("type".to_owned(), Value::from("choice"));
            if let Some(instructions) = &choice.instructions {
                object.insert("instructions".to_owned(), instructions.clone());
            }
            let mut criteria = Map::new();
            for (label, description) in &choice.criteria {
                criteria.insert(label.clone(), description.clone());
            }
            object.insert("criteria".to_owned(), Value::Object(criteria));
        }
        Question::Noul(noul) => {
            object.insert("type".to_owned(), Value::from("noul"));
            if let Some(instructions) = &noul.instructions {
                object.insert("instructions".to_owned(), instructions.clone());
            }
            if noul.true_description.is_some() || noul.false_description.is_some() {
                let mut criteria = Map::new();
                if let Some(description) = &noul.true_description {
                    criteria.insert("true".to_owned(), description.clone());
                }
                if let Some(description) = &noul.false_description {
                    criteria.insert("false".to_owned(), description.clone());
                }
                object.insert("criteria".to_owned(), Value::Object(criteria));
            }
        }
        Question::Score(score) => {
            object.insert("type".to_owned(), Value::from("score"));
            if let Some(instructions) = &score.instructions {
                object.insert("instructions".to_owned(), instructions.clone());
            }
            object.insert("criteria".to_owned(), Value::Array(score.levels.clone()));
        }
    }
    Value::Object(object)
}

/// Parse a Jev-compatible response body into a neutral
/// [`DecisionResponse`]. Structure is strict; unknown top-level and answer
/// fields are permitted upstream extensions and ignored, except `id`, which
/// is captured as the provider request id. An `id` longer than
/// [`MAX_PROVIDER_REQUEST_ID_BYTES`] is dropped instead of rejected: the
/// body is already paid for, and a truncated id would name a request that
/// does not exist. Range and normalization checks stay with the host, which
/// validates before returning.
pub fn parse_response(bytes: &[u8]) -> Result<DecisionResponse, WireError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| WireError::bad_json("response body must be valid UTF-8 JSON"))?;
    let value = parse_strict(text)?;
    let mut response = into_object(value, "response")?;
    let model = match response.shift_remove("model") {
        Some(Value::String(model)) if !model.is_empty() => model,
        Some(_) => {
            return Err(WireError::validation(
                "response.model must be a nonempty string",
            ));
        }
        None => return Err(WireError::validation("response.model is required")),
    };
    let provider_request_id = match response.shift_remove("id") {
        Some(Value::String(id)) => (id.len() <= MAX_PROVIDER_REQUEST_ID_BYTES).then_some(id),
        None | Some(Value::Null) => None,
        Some(_) => {
            return Err(WireError::validation(
                "response.id must be a string or null when provided",
            ));
        }
    };
    // OpenRouter names the provider that served the request. It is an
    // extension: kept when it is a short string, otherwise ignored.
    let upstream_provider = match response.shift_remove("provider") {
        Some(Value::String(provider)) if provider.len() <= MAX_PROVIDER_REQUEST_ID_BYTES => {
            Some(provider)
        }
        _ => None,
    };
    let usage = match response.shift_remove("usage") {
        None | Some(Value::Null) => systemone_core::Usage::default(),
        Some(value) => parse_usage(value)?,
    };
    let answers = match response.shift_remove("answers") {
        Some(value) => into_object(value, "response.answers")?,
        None => return Err(WireError::validation("response.answers is required")),
    };
    // Remaining fields are permitted upstream extensions.
    let mut parsed = Vec::with_capacity(answers.len());
    for (id, answer) in answers {
        parsed.push((id.clone(), parse_answer(&id, answer)?));
    }
    Ok(DecisionResponse {
        model,
        answers: parsed,
        usage,
        diagnostics: systemone_core::Diagnostics {
            provider_request_id,
            upstream_provider,
            ..systemone_core::Diagnostics::default()
        },
    })
}

fn parse_usage(value: Value) -> Result<systemone_core::Usage, WireError> {
    let mut usage = into_object(value, "response.usage")?;
    let input_tokens = optional_u64(
        usage.shift_remove("input_tokens"),
        "response.usage.input_tokens",
    )?;
    let output_tokens = optional_u64(
        usage.shift_remove("output_tokens"),
        "response.usage.output_tokens",
    )?;
    let cost = match usage.shift_remove("cost") {
        None | Some(Value::Null) => None,
        Some(value) => Some(require_number(&value, "response.usage.cost")?),
    };
    Ok(systemone_core::Usage {
        input_tokens,
        output_tokens,
        cost,
    })
}

fn optional_u64(value: Option<Value>, path: &str) -> Result<Option<u64>, WireError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| WireError::validation(format!("{path} must be a nonnegative integer"))),
    }
}

fn require_number(value: &Value, path: &str) -> Result<f64, WireError> {
    value
        .as_f64()
        .ok_or_else(|| WireError::validation(format!("{path} must be a number")))
}

fn optional_confidence(value: Option<Value>, path: &str) -> Result<Option<f64>, WireError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => require_number(&value, path).map(Some),
    }
}

fn parse_answer(id: &str, value: Value) -> Result<Answer, WireError> {
    let path = format!("response.answers.{id:?}");
    let mut answer = into_object(value, &path)?;
    let kind = match answer.shift_remove("type") {
        Some(Value::String(kind)) => kind,
        Some(_) => {
            return Err(WireError::validation(format!(
                "{path}.type must be a string"
            )));
        }
        None => return Err(WireError::validation(format!("{path}.type is required"))),
    };
    match kind.as_str() {
        "choice" => {
            let confidence = optional_confidence(
                answer.shift_remove("confidence"),
                &format!("{path}.confidence"),
            )?;
            let choice = match answer.shift_remove("choice") {
                Some(Value::String(choice)) => choice,
                Some(_) => {
                    return Err(WireError::validation(format!(
                        "{path}.choice must be a string"
                    )));
                }
                None => {
                    return Err(WireError::validation(format!(
                        "{path}.choice is required for choice"
                    )));
                }
            };
            let probabilities = match answer.shift_remove("probabilities") {
                Some(value) => into_object(value, &format!("{path}.probabilities"))?,
                None => {
                    return Err(WireError::validation(format!(
                        "{path}.probabilities is required for choice"
                    )));
                }
            };
            let mut pairs = Vec::with_capacity(probabilities.len());
            for (label, probability) in probabilities {
                let number =
                    require_number(&probability, &format!("{path}.probabilities.{label:?}"))?;
                pairs.push((label, number));
            }
            Ok(Answer::Choice(systemone_core::ChoiceAnswer {
                choice,
                probabilities: pairs,
                confidence,
            }))
        }
        "noul" => {
            let probability_true = match answer.shift_remove("noul") {
                Some(value) => require_number(&value, &format!("{path}.noul"))?,
                None => {
                    return Err(WireError::validation(format!(
                        "{path}.noul is required for noul"
                    )));
                }
            };
            // A noul answer has no confidence field, so an upstream
            // `confidence` here is an ignored extension like any other
            // unknown answer field. It is not read and not rejected.
            Ok(Answer::Noul(systemone_core::NoulAnswer {
                probability_true,
            }))
        }
        "score" => parse_score_answer(&path, answer),
        _ => Err(WireError::validation(format!(
            "{path}.type must be choice, noul, or score"
        ))),
    }
}

fn parse_score_answer(path: &str, mut answer: Map<String, Value>) -> Result<Answer, WireError> {
    let confidence = optional_confidence(
        answer.shift_remove("confidence"),
        &format!("{path}.confidence"),
    )?;
    let score = match answer.shift_remove("score") {
        Some(value) => require_number(&value, &format!("{path}.score"))?,
        None => {
            return Err(WireError::validation(format!(
                "{path}.score is required for score"
            )));
        }
    };
    let legend = match answer.shift_remove("legend") {
        Some(value) => into_object(value, &format!("{path}.legend"))?,
        None => {
            return Err(WireError::validation(format!(
                "{path}.legend is required for score"
            )));
        }
    };
    let probabilities = match answer.shift_remove("probabilities") {
        Some(value) => into_object(value, &format!("{path}.probabilities"))?,
        None => {
            return Err(WireError::validation(format!(
                "{path}.probabilities is required for score"
            )));
        }
    };
    let legend_path = format!("{path}.legend");
    let mut levels: Vec<(u64, Value)> = Vec::with_capacity(legend.len());
    for (index, description) in legend {
        levels.push((parse_level_index(&index, &legend_path)?, description));
    }
    levels.sort_by_key(|(index, _)| *index);
    require_contiguous_levels(levels.iter().map(|(index, _)| *index), &legend_path)?;
    let probabilities_path = format!("{path}.probabilities");
    let mut numbers: Vec<(u64, f64)> = Vec::with_capacity(probabilities.len());
    for (index, probability) in probabilities {
        let index = parse_level_index(&index, &probabilities_path)?;
        let number = require_number(&probability, &format!("{probabilities_path}.{index}"))?;
        numbers.push((index, number));
    }
    numbers.sort_by_key(|(index, _)| *index);
    require_contiguous_levels(numbers.iter().map(|(index, _)| *index), &probabilities_path)?;
    if levels.len() != numbers.len() {
        return Err(WireError::validation(format!(
            "{path}.legend and probabilities must cover the same levels"
        )));
    }
    Ok(Answer::Score(systemone_core::ScoreAnswer {
        score,
        probabilities: numbers.into_iter().map(|(_, number)| number).collect(),
        confidence,
        legend: levels
            .into_iter()
            .map(|(_, description)| description)
            .collect(),
    }))
}

/// Parse one score level key. A key is a canonical zero-based decimal
/// index: ASCII digits only, no sign, no space, and no leading zero.
/// `"01"` is not the same key as `"1"` on the wire, so it is refused
/// instead of being folded into level 1.
fn parse_level_index(key: &str, path: &str) -> Result<u64, WireError> {
    let canonical = !key.is_empty()
        && key.bytes().all(|byte| byte.is_ascii_digit())
        && (key == "0" || !key.starts_with('0'));
    if !canonical {
        return Err(WireError::validation(format!(
            "{path} keys must be contiguous zero-based level indexes"
        )));
    }
    key.parse::<u64>().map_err(|_| {
        WireError::validation(format!(
            "{path} keys must be contiguous zero-based level indexes"
        ))
    })
}

/// Put a parsed response in the key order the request declared.
///
/// A hosted upstream chooses its own key order. SystemOne answers in one
/// order for every backend: `answers` follows the request's question
/// order, and each choice distribution follows the order the request
/// declared its labels in. This function moves entries only. It never
/// changes, adds or drops a value, and it never renormalizes.
///
/// The response must answer exactly the questions that were asked, with
/// exactly the labels that were declared, and with the primitive each
/// question asked for. A missing, extra, renamed or retyped entry is an
/// invalid upstream body: the caller refuses it and never repairs it.
///
/// Score answers are positional already, so they need no reordering, but
/// they must cover exactly the levels the request declared. The level
/// descriptions themselves pass through as the upstream reported them.
///
/// The function takes the response by value and returns the aligned one, so
/// a refused body cannot leave a half-moved response behind.
pub fn align_to_request(
    mut response: DecisionResponse,
    request: &DecisionRequest,
) -> Result<DecisionResponse, WireError> {
    let mut supplied = take_unique(
        std::mem::take(&mut response.answers),
        "response.answers",
        "answer",
    )?;
    let mut ordered = Vec::with_capacity(request.questions.len());
    for (id, question) in &request.questions {
        let mut answer = supplied.remove(id).ok_or_else(|| {
            WireError::validation(format!(
                "response.answers has no answer for question {id:?}"
            ))
        })?;
        align_answer(id, question, &mut answer)?;
        ordered.push((id.clone(), answer));
    }
    if let Some(extra) = smallest_key(&supplied) {
        return Err(WireError::validation(format!(
            "response.answers has answer {extra:?} for a question the request did not ask"
        )));
    }
    response.answers = ordered;
    Ok(response)
}

fn align_answer(id: &str, question: &Question, answer: &mut Answer) -> Result<(), WireError> {
    let path = format!("response.answers.{id:?}");
    match (question, answer) {
        (Question::Choice(question), Answer::Choice(answer)) => {
            let mut supplied = take_unique(
                std::mem::take(&mut answer.probabilities),
                &format!("{path}.probabilities"),
                "label",
            )?;
            let mut ordered = Vec::with_capacity(question.criteria.len());
            for (label, _) in &question.criteria {
                let probability = supplied.remove(label).ok_or_else(|| {
                    WireError::validation(format!(
                        "{path}.probabilities has no entry for the declared label {label:?}"
                    ))
                })?;
                ordered.push((label.clone(), probability));
            }
            if let Some(extra) = smallest_key(&supplied) {
                return Err(WireError::validation(format!(
                    "{path}.probabilities has label {extra:?}, which the request did not declare"
                )));
            }
            answer.probabilities = ordered;
            Ok(())
        }
        (Question::Score(question), Answer::Score(answer)) => {
            let declared = question.levels.len();
            for (field, covered) in [
                ("legend", answer.legend.len()),
                ("probabilities", answer.probabilities.len()),
            ] {
                if covered != declared {
                    return Err(WireError::validation(format!(
                        "{path}.{field} covers {covered} levels, but the request declared {declared}"
                    )));
                }
            }
            Ok(())
        }
        (Question::Noul(_), Answer::Noul(_)) => Ok(()),
        (question, answer) => Err(WireError::validation(format!(
            "{path}.type is {}, but the request asked {}",
            answer_primitive(answer),
            question.primitive().as_str()
        ))),
    }
}

/// Index entries by key. A duplicate key is refused instead of dropping
/// one of the two values silently.
fn take_unique<T>(
    entries: Vec<(String, T)>,
    path: &str,
    noun: &str,
) -> Result<HashMap<String, T>, WireError> {
    let mut indexed = HashMap::with_capacity(entries.len());
    for (key, value) in entries {
        if indexed.insert(key.clone(), value).is_some() {
            return Err(WireError::validation(format!(
                "{path} has duplicate {noun} {key:?}"
            )));
        }
    }
    Ok(indexed)
}

/// Name one leftover key. The smallest keeps the message deterministic.
fn smallest_key<T>(entries: &HashMap<String, T>) -> Option<&String> {
    entries.keys().min()
}

const fn answer_primitive(answer: &Answer) -> &'static str {
    match answer {
        Answer::Choice(_) => "choice",
        Answer::Noul(_) => "noul",
        Answer::Score(_) => "score",
    }
}

/// Require sorted level indexes to be exactly `0..n-1`. Sparse keys such as
/// `{"0","2"}` name a scale that the positional vectors cannot hold, so the
/// parser refuses them instead of renumbering the levels.
fn require_contiguous_levels(
    indexes: impl Iterator<Item = u64>,
    path: &str,
) -> Result<(), WireError> {
    for (position, index) in indexes.enumerate() {
        if index != position as u64 {
            return Err(WireError::validation(format!(
                "{path} keys must be contiguous zero-based level indexes"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use systemone_core::{ChoiceAnswer, Diagnostics, NoulAnswer, ScoreAnswer, Usage};

    use super::*;

    #[test]
    fn parses_all_types_preserving_order_values_and_backend_selector() {
        let parsed = parse_request(br#"{
          "model":"jev-latest","backend":"cloud","state":{"z":1.5,"a":null},"questions":{
            "\u96ea":{"type":"choice","instructions":{"task":"pick"},"criteria":{"b":null,"a":{"why":"A"}}},
            "truth":{"type":"noul","criteria":{"true":{"means":"yes"},"false":"Nope"}},
            "score":{"type":"score","instructions":[],"criteria":[null,{"level":"high"}]}
          }
        }"#).unwrap();
        assert_eq!(parsed.backend.as_deref(), Some("cloud"));
        assert_eq!(parsed.request.model.as_deref(), Some("jev-latest"));
        assert_eq!(parsed.request.state["z"], 1.5);
        assert_eq!(parsed.request.questions[0].0, "雪");
        match &parsed.request.questions[0].1 {
            Question::Choice(choice) => {
                assert_eq!(choice.criteria[0].0, "b");
                assert_eq!(choice.criteria[1].1["why"], "A");
            }
            other => panic!("{other:?}"),
        }
        match &parsed.request.questions[1].1 {
            Question::Noul(noul) => assert_eq!(noul.false_description, Some("Nope".into())),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rejects_structural_errors() {
        for body in [
            br#"{"state":"s","questions":{},"extra":null}"#.as_slice(),
            br#"{"state":"s","questions":{"q":{"type":"choice","criteria":{}}}}"#,
            br#"{"state":"s","questions":{"q":{"type":"score","criteria":["one"]}}}"#,
            br#"{"state":"s","questions":{"q":{"type":"noul","bogus":1}}}"#,
            br#"{"state":true,"questions":{"q":{"type":"noul"}}}"#,
            br#"{"backend":"","state":"s","questions":{"q":{"type":"noul"}}}"#,
        ] {
            assert_eq!(
                parse_request(body).unwrap_err().error_type,
                "validation_error",
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        assert_eq!(
            parse_request(br#"{"state":"s","state":"x","questions":{"q":{"type":"noul"}}}"#)
                .unwrap_err()
                .error_type,
            "validation_error"
        );
        assert_eq!(parse_request(br"{").unwrap_err().error_type, "invalid_json");
        assert_eq!(
            parse_request(br#"{"state":"s","questions":{"q":{"type":"noul"}}} x"#)
                .unwrap_err()
                .error_type,
            "invalid_json"
        );
        let deep = format!(
            "{{\"state\":{}0{},\"questions\":{{\"q\":{{\"type\":\"noul\"}}}}}}",
            "[".repeat(70),
            "]".repeat(70)
        );
        assert_eq!(
            parse_request(deep.as_bytes()).unwrap_err().error_type,
            "validation_error"
        );
    }

    #[test]
    fn renders_and_parses_hosted_round_trip() {
        let body = br#"{
          "backend":"cloud","model":"jev-latest",
          "state":{"z":1.5,"a":null},
          "questions":{
            "pick":{"type":"choice","instructions":{"task":"pick"},"criteria":{"b":null,"a":{"why":"A"}}},
            "truth":{"type":"noul","criteria":{"true":{"means":"yes"},"false":"Nope"}},
            "rate":{"type":"score","instructions":[],"criteria":[null,{"level":"high"}]}
          }
        }"#;
        let parsed = parse_request(body).unwrap();
        let rendered = render_request(&parsed.request, "jev-latest");
        // Routing selectors never leave the service.
        assert!(rendered.get("backend").is_none());
        assert_eq!(rendered["model"], "jev-latest");
        assert_eq!(rendered["state"]["z"], 1.5);
        assert_eq!(
            serde_json::to_string(&rendered).unwrap(),
            r#"{"model":"jev-latest","state":{"z":1.5,"a":null},"questions":{"pick":{"type":"choice","instructions":{"task":"pick"},"criteria":{"b":null,"a":{"why":"A"}}},"truth":{"type":"noul","criteria":{"true":{"means":"yes"},"false":"Nope"}},"rate":{"type":"score","instructions":[],"criteria":[null,{"level":"high"}]}}}"#
        );
        let response = parse_response(
            br#"{"id":"req-42","provider":"typesafe","model":"jev-latest",
              "answers":{
                "pick":{"type":"choice","choice":"a","confidence":0.9,
                        "probabilities":{"a":0.75,"b":0.25},"extra":1},
                "truth":{"type":"noul","noul":1.0},
                "rate":{"type":"score","score":2.0,"confidence":0.5,
                        "legend":{"1":"mid","0":"low","2":"high"},
                        "probabilities":{"2":0.6,"0":0.2,"1":0.2}}
              },
              "usage":{"input_tokens":30,"output_tokens":0,"cost":0.002}}"#,
        )
        .unwrap();
        assert_eq!(response.model, "jev-latest");
        assert_eq!(
            response.diagnostics.provider_request_id.as_deref(),
            Some("req-42")
        );
        assert_eq!(response.usage.input_tokens, Some(30));
        assert_eq!(response.usage.output_tokens, Some(0));
        assert_eq!(response.usage.cost, Some(0.002));
        match &response.answers[0].1 {
            Answer::Choice(choice) => {
                assert_eq!(choice.choice, "a");
                assert_eq!(choice.probabilities[0], ("a".to_owned(), 0.75));
                assert_eq!(choice.confidence, Some(0.9));
            }
            other => panic!("{other:?}"),
        }
        match &response.answers[2].1 {
            Answer::Score(score) => {
                assert_eq!(
                    score.legend,
                    vec![
                        serde_json::json!("low"),
                        serde_json::json!("mid"),
                        serde_json::json!("high")
                    ]
                );
                assert_eq!(score.probabilities, vec![0.2, 0.2, 0.6]);
            }
            other => panic!("{other:?}"),
        }
        assert!(response.validate(1e-6).is_ok());
        // The rendered response keeps the upstream order and values.
        let wire = serde_json::to_value(render_response(&response)).unwrap();
        assert_eq!(wire["answers"]["rate"]["legend"]["1"], "mid");
    }

    #[test]
    fn parse_response_rejects_structural_errors() {
        for (body, reason) in [
            (br#"{"answers":{}}"#.as_slice(), "model"),
            (br#"{"model":"","answers":{}}"#, "nonempty"),
            (br#"{"model":1,"answers":{}}"#, "string"),
            (br#"{"model":"m","answers":{"q":{"type":"noul"}}}"#, "noul"),
            (br#"{"model":"m","answers":{"q":{"type":"choice","choice":"a","probabilities":{"a":"x"}}}}"#, "number"),
            (br#"{"model":"m","answers":{"q":{"type":"bogus"}}}"#, "choice"),
            (
                br#"{"model":"m","answers":{"q":{"type":"score","score":0,"noul":1,"legend":{"0":"a","1":"b"},"probabilities":{"0":0.5}}}}"#,
                "same levels",
            ),
            (br#"{"model":"m","answers":{},"usage":{"input_tokens":-1}}"#, "nonnegative integer"),
            (br#"{"model":"m","answers":{},"id":{}}"#, "must be a string or null"),
            (br#"{"model":"m","answers":{},"id":7}"#, "must be a string or null"),
            (br#"{"model":"m","answers":[]}"#, "object"),
        ] {
            let error = parse_response(body).unwrap_err();
            assert!(error.message.contains(reason), "{} for {body:?}", error.message);
            assert_eq!(error.error_type, "validation_error");
        }
        assert!(parse_response(b"\xff").is_err());
    }

    #[test]
    fn parse_response_drops_an_over_long_id_and_keeps_the_body() {
        let long = "i".repeat(MAX_PROVIDER_REQUEST_ID_BYTES + 1);
        let body = format!(
            r#"{{"id":"{long}","model":"m","answers":{{"q":{{"type":"noul","noul":1.0}}}}}}"#
        );
        let response = parse_response(body.as_bytes()).unwrap();
        assert_eq!(response.diagnostics.provider_request_id, None);
        assert_eq!(response.answers.len(), 1);
        // The longest accepted id still arrives unchanged.
        let kept = "i".repeat(MAX_PROVIDER_REQUEST_ID_BYTES);
        let body = format!(
            r#"{{"id":"{kept}","model":"m","answers":{{"q":{{"type":"noul","noul":1.0}}}}}}"#
        );
        let response = parse_response(body.as_bytes()).unwrap();
        assert_eq!(
            response.diagnostics.provider_request_id.as_deref(),
            Some(kept.as_str())
        );
        // A non-string id is still a structural error, and the message
        // names the real problem.
        let error = parse_response(br#"{"id":5,"model":"m","answers":{}}"#).unwrap_err();
        assert_eq!(
            error.message,
            "response.id must be a string or null when provided"
        );
    }

    #[test]
    fn parse_response_requires_contiguous_zero_based_score_keys() {
        let answer = |legend: &str, probabilities: &str| {
            format!(
                r#"{{"model":"m","answers":{{"s":{{"type":"score","score":1,"legend":{legend},"probabilities":{probabilities}}}}}}}"#
            )
        };
        for (legend, probabilities) in [
            // Sparse keys name a scale the positional vectors cannot hold.
            (r#"{"0":"low","2":"high"}"#, r#"{"0":0.4,"2":0.6}"#),
            // Non-canonical decimals are not level keys.
            (r#"{"0":"low","01":"high"}"#, r#"{"0":0.4,"01":0.6}"#),
            (r#"{"0":"low","+1":"high"}"#, r#"{"0":0.4,"+1":0.6}"#),
            (r#"{"0":"low"," 1":"high"}"#, r#"{"0":0.4," 1":0.6}"#),
            (r#"{"0":"low","1x":"high"}"#, r#"{"0":0.4,"1x":0.6}"#),
            // One-based keys are renumbering, not a scale.
            (r#"{"1":"low","2":"high"}"#, r#"{"1":0.4,"2":0.6}"#),
            // A sparse probabilities map alone is refused too.
            (r#"{"0":"low","1":"high"}"#, r#"{"0":0.4,"2":0.6}"#),
        ] {
            let body = answer(legend, probabilities);
            let error = parse_response(body.as_bytes()).unwrap_err();
            assert_eq!(error.error_type, "validation_error");
            assert!(
                error
                    .message
                    .contains("contiguous zero-based level indexes"),
                "{} for {body}",
                error.message
            );
        }
        // Contiguous keys in any order stay accepted and keep level order.
        let body = answer(
            r#"{"2":"high","0":"low","1":"mid"}"#,
            r#"{"1":0.2,"2":0.6,"0":0.2}"#,
        );
        let response = parse_response(body.as_bytes()).unwrap();
        match &response.answers[0].1 {
            Answer::Score(score) => {
                assert_eq!(score.legend, vec!["low", "mid", "high"]);
                assert_eq!(score.probabilities, vec![0.2, 0.2, 0.6]);
            }
            other => panic!("{other:?}"),
        }
        // Matching key sets of different sizes keep the old message.
        let body = answer(r#"{"0":"low","1":"high"}"#, r#"{"0":1.0}"#);
        let error = parse_response(body.as_bytes()).unwrap_err();
        assert!(
            error.message.contains("must cover the same levels"),
            "{}",
            error.message
        );
    }

    #[test]
    fn parse_response_ignores_confidence_on_noul_like_any_other_extension() {
        // `NoulAnswer` has no confidence field. A numeric confidence is
        // dropped, so a non-numeric one must not reject the answer either.
        for confidence in ["0.9", r#""high""#, "null", "{}", "[1]"] {
            let body = format!(
                r#"{{"model":"m","answers":{{"n":{{"type":"noul","noul":0.25,"confidence":{confidence},"extra":1}}}}}}"#
            );
            let response = parse_response(body.as_bytes()).unwrap();
            match &response.answers[0].1 {
                Answer::Noul(noul) => assert_eq!(noul.probability_true, 0.25),
                other => panic!("{other:?}"),
            }
        }
        // Choice and score still validate confidence, because they keep it.
        for body in [
            br#"{"model":"m","answers":{"c":{"type":"choice","choice":"a","confidence":"high","probabilities":{"a":1.0}}}}"#.as_slice(),
            br#"{"model":"m","answers":{"s":{"type":"score","score":0,"confidence":"high","legend":{"0":"low","1":"high"},"probabilities":{"0":0.5,"1":0.5}}}}"#,
        ] {
            let error = parse_response(body).unwrap_err();
            assert!(
                error.message.contains("confidence must be a number"),
                "{}",
                error.message
            );
        }
    }

    #[test]
    fn renders_rounded_wire_values_and_omits_unknown_usage() {
        let response = DecisionResponse {
            model: "m".into(),
            answers: vec![
                (
                    "c".into(),
                    Answer::Choice(ChoiceAnswer {
                        choice: "first".into(),
                        probabilities: vec![
                            ("first".into(), 1.0 / 3.0),
                            ("second".into(), 2.0 / 3.0),
                        ],
                        confidence: Some(0.333),
                    }),
                ),
                (
                    "n".into(),
                    Answer::Noul(NoulAnswer {
                        probability_true: 0.126,
                    }),
                ),
                (
                    "s".into(),
                    Answer::Score(ScoreAnswer {
                        score: 1.415,
                        probabilities: vec![0.126, 0.333, 0.541],
                        confidence: None,
                        legend: vec!["low".into(), "mid".into(), "high".into()],
                    }),
                ),
            ],
            usage: Usage {
                input_tokens: Some(30),
                output_tokens: None,
                cost: None,
            },
            diagnostics: Diagnostics::default(),
        };
        let body = serde_json::to_value(render_response(&response)).unwrap();
        assert_eq!(body["answers"]["c"]["probabilities"]["first"], 0.33);
        assert_eq!(body["answers"]["c"]["probabilities"]["second"], 0.67);
        assert_eq!(body["answers"]["c"]["confidence"], 0.33);
        assert_eq!(body["answers"]["n"]["noul"], 0.13);
        assert_eq!(body["answers"]["s"]["score"], 1.42);
        assert_eq!(body["answers"]["s"]["legend"]["1"], "mid");
        assert!(body["answers"]["s"].get("confidence").is_none());
        assert_eq!(body["usage"]["input_tokens"], 30);
        assert!(body["usage"].get("output_tokens").is_none());
        let keys: Vec<_> = body["answers"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["c", "n", "s"]);
    }

    /// Key order is normalized; values are not. A response that answers
    /// other questions or other labels is refused, never reshaped.
    #[test]
    fn alignment_reorders_keys_and_refuses_a_different_answer_set() {
        let request = parse_request(
            br#"{"state":"s","questions":{
              "pick":{"type":"choice","criteria":{"alpha":null,"beta":null}},
              "worth":{"type":"noul"}
            }}"#,
        )
        .unwrap()
        .request;
        let upstream = br#"{"model":"m","answers":{
          "worth":{"type":"noul","noul":0.62},
          "pick":{"type":"choice","choice":"beta","probabilities":{"beta":0.75,"alpha":0.25}}
        }}"#;

        let response = align_to_request(parse_response(upstream).unwrap(), &request).unwrap();
        let answered: Vec<&str> = response.answers.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(answered, ["pick", "worth"]);
        let Answer::Choice(choice) = &response.answers[0].1 else {
            panic!("expected a choice answer");
        };
        assert_eq!(
            choice.probabilities,
            vec![("alpha".to_owned(), 0.25), ("beta".to_owned(), 0.75)]
        );

        for body in [
            // A label the request did not declare.
            br#"{"model":"m","answers":{"pick":{"type":"choice","choice":"alpha","probabilities":{"alpha":0.5,"gamma":0.5}},"worth":{"type":"noul","noul":0.1}}}"#.as_slice(),
            // A question the request did not ask.
            br#"{"model":"m","answers":{"pick":{"type":"choice","choice":"alpha","probabilities":{"alpha":0.5,"beta":0.5}},"worth":{"type":"noul","noul":0.1},"spare":{"type":"noul","noul":0.1}}}"#,
            // A missing answer.
            br#"{"model":"m","answers":{"pick":{"type":"choice","choice":"alpha","probabilities":{"alpha":0.5,"beta":0.5}}}}"#,
            // The wrong primitive for a declared question.
            br#"{"model":"m","answers":{"pick":{"type":"noul","noul":0.5},"worth":{"type":"noul","noul":0.1}}}"#,
        ] {
            let error =
                align_to_request(parse_response(body).unwrap(), &request).unwrap_err();
            assert_eq!(
                error.error_type,
                "validation_error",
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// A score answer is positional, so it must cover exactly the levels
    /// the request declared. A shorter or longer rubric answers another
    /// question; it is refused, not accepted with the upstream's scale.
    #[test]
    fn alignment_refuses_a_score_answer_with_another_level_count() {
        let request = parse_request(
            br#"{"state":"s","questions":{
              "urgency":{"type":"score","criteria":["low","medium","high"]}
            }}"#,
        )
        .unwrap()
        .request;

        let matching = br#"{"model":"m","answers":{"urgency":{"type":"score","score":1,
          "legend":{"0":"low","1":"medium","2":"high"},
          "probabilities":{"0":0.2,"1":0.5,"2":0.3}}}}"#;
        let response = align_to_request(parse_response(matching).unwrap(), &request).unwrap();
        let Answer::Score(score) = &response.answers[0].1 else {
            panic!("expected a score answer");
        };
        assert_eq!(score.probabilities.len(), 3);

        for body in [
            // Fewer levels than the request declared.
            br#"{"model":"m","answers":{"urgency":{"type":"score","score":1,"legend":{"0":"low","1":"high"},"probabilities":{"0":0.5,"1":0.5}}}}"#.as_slice(),
            // More levels than the request declared.
            br#"{"model":"m","answers":{"urgency":{"type":"score","score":1,"legend":{"0":"a","1":"b","2":"c","3":"d"},"probabilities":{"0":0.25,"1":0.25,"2":0.25,"3":0.25}}}}"#,
        ] {
            let error =
                align_to_request(parse_response(body).unwrap(), &request).unwrap_err();
            assert_eq!(
                error.error_type,
                "validation_error",
                "{}",
                String::from_utf8_lossy(body)
            );
            assert!(
                error.message.contains("the request declared 3"),
                "{}",
                error.message
            );
        }
    }
}
