use axum::{Json, Router, routing::post};
use color_eyre::eyre::Result;
use tracing::info;

async fn echo(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
    Json(body)
}

fn app() -> Router {
    Router::new().route("/echo", post(echo))
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let port: u16 = std::env::var("BRIDGE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    info!(%addr, "bridge server listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app()).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn echo_returns_body() {
        let body = serde_json::json!({"hello": "world"});
        let req = Request::post("/echo")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = app().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result, body);
    }

    #[tokio::test]
    async fn echo_returns_nested_json() {
        let body = serde_json::json!({"a": [1, 2], "b": {"c": true}});
        let req = Request::post("/echo")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = app().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result, body);
    }

    #[tokio::test]
    async fn echo_rejects_non_json() {
        let req = Request::post("/echo")
            .header("content-type", "text/plain")
            .body(Body::from("not json"))
            .unwrap();

        let resp = app().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn get_echo_not_allowed() {
        let req = Request::get("/echo").body(Body::empty()).unwrap();

        let resp = app().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
