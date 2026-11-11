//! Gateway plumbing shared by every API that is backed by the privileged
//! agent: error mapping and the `/api/agent/status` endpoint.
//!
//! The web process holds no privileges. Everything privileged goes through
//! [`AgentClient`], which can only send requests to the agent's plugins; the
//! agent validates them again.

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use controldeck_proto::{AgentClient, ErrorCode, RpcError};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    running_job: Option<Value>,
}

/// An agent error rendered as an HTTP response.
pub struct ApiError(pub RpcError);

impl From<RpcError> for ApiError {
    fn from(e: RpcError) -> Self {
        ApiError(e)
    }
}

pub fn status_for(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::BadRequest => StatusCode::BAD_REQUEST,
        ErrorCode::Forbidden => StatusCode::FORBIDDEN,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::Conflict => StatusCode::CONFLICT,
        ErrorCode::Unsupported => StatusCode::NOT_IMPLEMENTED,
        ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        ErrorCode::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let running_job = self.0.data.as_ref().and_then(|d| d.get("running_job")).cloned();
        (status_for(self.0.code), Json(ErrorBody { error: self.0.message, running_job })).into_response()
    }
}

/// `GET /api/agent/status`: which plugins the agent offers (503 when it is down).
async fn status(State(agent): State<AgentClient>) -> Result<Json<Value>, ApiError> {
    let mut v = agent.call("agent", "modules", Value::Null).await?;
    v["connected"] = json!(true);
    Ok(Json(v))
}

pub fn router<S: Clone + Send + Sync + 'static>(agent: AgentClient) -> Router<S> {
    Router::new().route("/api/agent/status", get(status)).with_state(agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_code_has_a_status() {
        assert_eq!(status_for(ErrorCode::BadRequest), StatusCode::BAD_REQUEST);
        assert_eq!(status_for(ErrorCode::Forbidden), StatusCode::FORBIDDEN);
        assert_eq!(status_for(ErrorCode::NotFound), StatusCode::NOT_FOUND);
        assert_eq!(status_for(ErrorCode::Conflict), StatusCode::CONFLICT);
        assert_eq!(status_for(ErrorCode::Unsupported), StatusCode::NOT_IMPLEMENTED);
        assert_eq!(status_for(ErrorCode::Internal), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(status_for(ErrorCode::Unavailable), StatusCode::SERVICE_UNAVAILABLE);
    }
}
