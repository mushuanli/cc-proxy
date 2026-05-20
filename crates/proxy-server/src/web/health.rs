use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

pub async fn check() -> impl IntoResponse {
    Json(json!({"status": "ok"})).into_response()
}
