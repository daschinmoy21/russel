//! HTTP handlers for the secrets API (`/secrets`, `/secrets/{name}`).

use axum::{Json, extract::Path, http::StatusCode};

#[derive(Debug, serde::Deserialize)]
pub(super) struct SecretSetBody {
    value: String,
}

#[derive(Debug, serde::Serialize)]
pub(super) struct SecretsListResponse {
    secrets: Vec<String>,
}

pub(super) async fn secrets_list() -> Result<Json<SecretsListResponse>, (StatusCode, String)> {
    crate::secrets::list_secrets()
        .map(|secrets| Json(SecretsListResponse { secrets }))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub(super) async fn secrets_set(
    Path(name): Path<String>,
    Json(body): Json<SecretSetBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Validation failures (bad name/value) → 400 BAD_REQUEST.
    crate::secrets::validate_secret_name(&name)
        .and_then(|_| crate::secrets::validate_secret_value(&body.value))
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    // Downstream store errors (fs) → 500 INTERNAL_SERVER_ERROR.
    let result = crate::secrets::set_secret(&name, &body.value)
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    if result.is_ok() {
        // Audit log — name only, never the secret value.
        tracing::info!(secret = %name, "secret set via API");
    }
    result
}

pub(super) async fn secrets_delete(
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Validation failures (bad name) → 400 BAD_REQUEST.
    crate::secrets::validate_secret_name(&name)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    match crate::secrets::delete_secret(&name) {
        Ok(true) => {
            // Audit log — name only, never the secret value.
            tracing::info!(secret = %name, "secret deleted via API");
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(false) => Err((StatusCode::NOT_FOUND, format!("secret {name:?} not found"))),
        // Downstream store errors (fs) → 500 INTERNAL_SERVER_ERROR.
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}
