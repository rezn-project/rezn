use reqwest::StatusCode;

pub type AppError = (StatusCode, String);

pub fn app_error<E: std::fmt::Display>(e: E) -> AppError {
    tracing::warn!("internal error: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
