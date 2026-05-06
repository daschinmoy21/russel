use axum::body::Body;
use tokio_stream::StreamExt;
fn test() {
    let _b = Body::from_stream(tokio_stream::empty::<Result<axum::body::Bytes, std::convert::Infallible>>());
}
