//! Dispatches Tool calls that require Cursor user interaction.
//! Interaction query dispatch and approval continuation.

use crate::{
    cursor::{protocol::proto::agent::v1 as pb, tools::codec as interaction},
    model::ToolCall,
    search::{WebFetch, WebSearch},
    store::Store,
    Error, Result,
};

use super::{normalized, InteractionContinuation, ToolStart};
use crate::cursor::tools::{
    runtime::{CursorToolRuntime, PendingInteraction},
    tool_call_result::{self as result, ToolResultSender},
};

pub(super) async fn start(
    runtime: &CursorToolRuntime,
    call: &ToolCall,
    conversation_id: &str,
) -> Result<ToolStart> {
    let id = runtime.reserve_interaction(call, conversation_id).await?;
    Ok(ToolStart {
        messages: vec![interaction::tool_query(id, call)?],
        completion: None,
    })
}

pub(super) async fn resume(
    results: &ToolResultSender,
    search: &WebSearch,
    fetch: &WebFetch,
    store: Option<Store>,
    pending: PendingInteraction,
    response: &pb::InteractionResponse,
) -> Result<InteractionContinuation> {
    if normalized(&pending.call.name) == "websearch"
        && matches!(
            response.result.as_ref(),
            Some(pb::interaction_response::Result::WebSearchRequestResponse(
                pb::WebSearchRequestResponse {
                    result: Some(pb::web_search_request_response::Result::Approved(_)),
                }
            ))
        )
    {
        start_web_search(results.clone(), search.clone(), pending)?;
        return Ok(InteractionContinuation::Pending);
    }
    if normalized(&pending.call.name) == "webfetch"
        && matches!(
            response.result.as_ref(),
            Some(pb::interaction_response::Result::WebFetchRequestResponse(
                pb::WebFetchRequestResponse {
                    result: Some(pb::web_fetch_request_response::Result::Approved(_)),
                }
            ))
        )
    {
        start_web_fetch(results.clone(), fetch.clone(), pending)?;
        return Ok(InteractionContinuation::Pending);
    }
    if normalized(&pending.call.name) == "generateimage"
        && matches!(
            response.result.as_ref(),
            Some(
                pb::interaction_response::Result::GenerateImageRequestResponse(
                    pb::GenerateImageRequestResponse {
                        result: Some(pb::generate_image_request_response::Result::Approved(_)),
                    }
                )
            )
        )
    {
        start_generate_image(results.clone(), store, pending)?;
        return Ok(InteractionContinuation::Pending);
    }
    Ok(InteractionContinuation::Completed(Box::new(
        result::from_interaction(pending, response)?,
    )))
}

fn start_web_fetch(
    results: ToolResultSender,
    fetch: WebFetch,
    pending: PendingInteraction,
) -> Result<()> {
    let url = pending
        .call
        .arguments
        .get("url")
        .and_then(serde_json::Value::as_str)
        .filter(|url| !url.trim().is_empty())
        .ok_or_else(|| Error::Protocol("WebFetch is missing url".into()))?
        .to_string();
    tokio::spawn(async move {
        let outcome = fetch.fetch(&url).await.map_err(|error| error.to_string());
        match result::complete_web_fetch(pending, outcome) {
            Ok(completion) => results.send(completion),
            Err(error) => results.send_error(error),
        }
    });
    Ok(())
}

fn start_generate_image(
    results: ToolResultSender,
    store: Option<Store>,
    pending: PendingInteraction,
) -> Result<()> {
    let description = pending
        .call
        .arguments
        .get("description")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Error::Protocol("GenerateImage is missing description".into()))?
        .to_string();
    let references = match pending.call.arguments.get("reference_image_paths") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Array(values)) => !values.is_empty(),
        _ => {
            return Err(Error::Protocol(
                "GenerateImage reference_image_paths must be an array".into(),
            ))
        }
    };
    if references {
        return Err(Error::Protocol(
            "GenerateImage reference images are not supported by this version".into(),
        ));
    }
    let aspect_ratio = match pending.call.arguments.get("aspect_ratio") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) => {
            (!value.trim().is_empty()).then(|| value.trim().to_owned())
        }
        _ => {
            return Err(Error::Protocol(
                "GenerateImage aspect_ratio must be a string".into(),
            ))
        }
    };
    let Some(store) = store else {
        return Err(Error::Protocol(
            "GenerateImage requires application storage".into(),
        ));
    };
    tokio::spawn(async move {
        let outcome = crate::cursor::services::images::generate(
            &store,
            &pending.conversation_id,
            &description,
            aspect_ratio.as_deref(),
        )
        .await
        .map_err(|error| error.to_string());
        match result::complete_generate_image(pending, outcome) {
            Ok(completion) => results.send(completion),
            Err(error) => results.send_error(error),
        }
    });
    Ok(())
}

fn start_web_search(
    results: ToolResultSender,
    search: WebSearch,
    pending: PendingInteraction,
) -> Result<()> {
    // Claude Code 习惯的 query 作为 search_term 的别名兼容。
    let query = ["search_term", "query"]
        .iter()
        .find_map(|name| {
            pending
                .call
                .arguments
                .get(name)
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
        })
        .ok_or_else(|| Error::Protocol("WebSearch is missing search_term".into()))?
        .to_string();
    tokio::spawn(async move {
        let outcome = search
            .search(&query)
            .await
            .map_err(|error| error.to_string());
        match result::complete_web_search(pending, outcome) {
            Ok(completion) => results.send(completion),
            Err(error) => results.send_error(error),
        }
    });
    Ok(())
}
