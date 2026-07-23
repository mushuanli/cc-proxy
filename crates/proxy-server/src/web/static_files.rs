use axum::body::Body;
use axum::response::Response;
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../wwwroot"]
struct Assets;

pub async fn serve(uri: axum::http::Uri) -> Response<Body> {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .header("content-type", mime.as_ref())
                .body(Body::from(file.data.to_vec()))
                .unwrap()
        }
        None => Assets::get("index.html")
            .map(|f| {
                Response::builder()
                    .header("content-type", "text/html")
                    .body(Body::from(f.data.to_vec()))
                    .unwrap()
            })
            .unwrap_or_else(|| {
                Response::builder()
                    .status(404)
                    .body(Body::from("Not Found"))
                    .unwrap()
            }),
    }
}
