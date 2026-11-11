//! One resample of an invalid tool call (`adapter.resampleOnInvalid`, H-1.6).
//!
//! When a reply holds a call the checks refuse, the model is not shown it. The same
//! request is sent once more at a higher temperature and labelled
//! (`ContextHint.reason = "resample"`) so the host can tell it from a rewrite of the
//! conversation. The reply to the resample is used as it is: a valid call runs, an
//! invalid one gets the typed error. There is never a second resample.

use vam_spec::v1::{InferenceRequest, TokenUsage, ToolCall};

use crate::inference::InferenceClient;

/// Temperature of a resample. The main request stays at its own (0.1).
pub const RESAMPLE_TEMPERATURE: f32 = 0.7;

/// What the resample returned.
#[derive(Debug, Default)]
pub struct Resampled {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Vec<TokenUsage>,
}

/// The request of the resample: the original, hotter and labelled. A recitation note
/// keeps its lane; only the reason says resample.
pub fn resample_request(original: &InferenceRequest) -> InferenceRequest {
    let mut request = original.clone();
    if let Some(pref) = request.model_preference.as_mut() {
        pref.temperature = RESAMPLE_TEMPERATURE;
    }
    let mut hint = request
        .context_hint
        .take()
        .unwrap_or(vam_spec::v1::ContextHint {
            lane: vam_spec::v1::context_hint::Lane::Main as i32,
            ..Default::default()
        });
    hint.reason = vam_spec::compaction::REASON_RESAMPLE.to_string();
    request.context_hint = Some(hint);
    request
}

/// Sends the resample and reads its reply. `None` when the call or the stream failed:
/// the caller then keeps the original reply, which is today's behaviour.
pub async fn again(client: &InferenceClient, original: &InferenceRequest) -> Option<Resampled> {
    let mut rx = client.infer(resample_request(original)).await.ok()?;
    let mut out = Resampled::default();
    while let Some(chunk) = rx.recv().await {
        let chunk = chunk.ok()?;
        if chunk.error.is_some() {
            return None;
        }
        out.text.push_str(&chunk.delta);
        out.tool_calls.extend(chunk.tool_calls);
        out.usage.extend(chunk.usage);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::v1::{ContextHint, ModelPreference};

    fn original(hint: Option<ContextHint>) -> InferenceRequest {
        InferenceRequest {
            model_preference: Some(ModelPreference {
                temperature: 0.1,
                ..Default::default()
            }),
            context_hint: hint,
            ..Default::default()
        }
    }

    #[test]
    fn the_resample_is_hotter_and_labelled_and_the_original_is_untouched() {
        let first = original(None);
        let again = resample_request(&first);
        assert_eq!(first.model_preference.as_ref().unwrap().temperature, 0.1);
        assert!(first.context_hint.is_none());
        assert_eq!(
            again.model_preference.unwrap().temperature,
            RESAMPLE_TEMPERATURE
        );
        let hint = again.context_hint.unwrap();
        assert_eq!(hint.lane, vam_spec::v1::context_hint::Lane::Main as i32);
        assert!(vam_spec::compaction::is_resample_hint(Some(&hint)));
        assert_eq!(again.messages, first.messages);
        assert_eq!(again.tools, first.tools);
    }

    #[test]
    fn a_recitation_note_keeps_its_lane() {
        let note = ContextHint {
            lane: vam_spec::v1::context_hint::Lane::Note as i32,
            ..Default::default()
        };
        let hint = resample_request(&original(Some(note)))
            .context_hint
            .unwrap();
        assert_eq!(hint.lane, vam_spec::v1::context_hint::Lane::Note as i32);
        assert_eq!(hint.reason, "resample");
    }
}
