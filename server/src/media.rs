use crate::api::{ApiError, AppState, UserView};
use axum::{
    body::Body,
    extract::{Extension, Path, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, SeekFrom},
};
use tokio_util::io::ReaderStream;

pub async fn stream(
    State(state): State<AppState>,
    Extension(_user): Extension<UserView>,
    Path(id): Path<String>,
    headers: HeaderMap,
    method: Method,
) -> Result<Response, ApiError> {
    let source = sqlx::query_as::<_, (String, i64, i64, String)>(
        "SELECT source_path,source_size,source_mtime_ms,source_status FROM videos WHERE id=?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::internal)?;
    let Some((relative, expected_size, expected_mtime, status)) = source else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Video not found"));
    };
    if status != "available" {
        return Err(ApiError::new(
            StatusCode::GONE,
            format!("Video source is {status}"),
        ));
    }
    let root = state.settings.source_dir.clone();
    let path = tokio::fs::canonicalize(root.join(relative))
        .await
        .map_err(|_| ApiError::new(StatusCode::GONE, "Video source is missing"))?;
    if !path.starts_with(&root) {
        return Err(ApiError::forbidden());
    }
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(ApiError::internal)?;
    let actual_mtime = metadata
        .modified()
        .map_err(ApiError::internal)?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    if metadata.len() as i64 != expected_size || actual_mtime != expected_mtime {
        return Err(ApiError::new(
            StatusCode::GONE,
            "Video changed since it was indexed. Ask an administrator to scan the library.",
        ));
    }
    serve_file(path, &headers, &method).await
}

pub async fn serve_file(
    path: std::path::PathBuf,
    headers: &HeaderMap,
    method: &Method,
) -> Result<Response, ApiError> {
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(ApiError::internal)?;
    if !metadata.is_file() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "File not found"));
    }
    let length = metadata.len();
    let range = headers
        .get(header::RANGE)
        .map(|value| value.to_str().map_err(ApiError::internal))
        .transpose()?;
    let selected = match parse_range(range, length) {
        Ok(value) => value,
        Err(()) => {
            let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            response.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{length}")).map_err(ApiError::internal)?,
            );
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, no-store"),
            );
            return Ok(response);
        }
    };
    let (status, start, end) = match selected {
        Some((start, end)) => (StatusCode::PARTIAL_CONTENT, start, end),
        None => (StatusCode::OK, 0, length.saturating_sub(1)),
    };
    let content_length = if length == 0 { 0 } else { end - start + 1 };
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    let response_headers = response.headers_mut();
    response_headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
    response_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response_headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response_headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response_headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&content_length.to_string()).map_err(ApiError::internal)?,
    );
    if status == StatusCode::PARTIAL_CONTENT {
        response_headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{length}"))
                .map_err(ApiError::internal)?,
        );
    }
    if *method != Method::HEAD && content_length > 0 {
        let mut file = File::open(path).await.map_err(ApiError::internal)?;
        file.seek(SeekFrom::Start(start))
            .await
            .map_err(ApiError::internal)?;
        let stream = ReaderStream::new(file.take(content_length));
        *response.body_mut() = Body::from_stream(stream);
    }
    Ok(response)
}

fn parse_range(value: Option<&str>, length: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some((unit, range)) = value.trim().split_once('=') else {
        return Err(());
    };
    if !unit.eq_ignore_ascii_case("bytes") || range.contains(',') || length == 0 {
        return Err(());
    }
    let Some((start, end)) = range.trim().split_once('-') else {
        return Err(());
    };
    if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        let count = suffix.min(length);
        return Ok(Some((length - count, length - 1)));
    }
    let start = start.parse::<u64>().map_err(|_| ())?;
    if start >= length {
        return Err(());
    }
    let end = if end.is_empty() {
        length - 1
    } else {
        end.parse::<u64>().map_err(|_| ())?.min(length - 1)
    };
    if end < start {
        return Err(());
    }
    Ok(Some((start, end)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::RANGE;
    use http_body_util::BodyExt;
    use std::io::Write;

    #[test]
    fn parses_open_closed_and_suffix_ranges() {
        assert_eq!(parse_range(Some("bytes=2-5"), 10), Ok(Some((2, 5))));
        assert_eq!(parse_range(Some("bytes=8-"), 10), Ok(Some((8, 9))));
        assert_eq!(parse_range(Some("bytes=-4"), 10), Ok(Some((6, 9))));
        assert_eq!(parse_range(None, 10), Ok(None));
    }

    #[test]
    fn rejects_unsatisfiable_and_multi_ranges() {
        assert!(parse_range(Some("bytes=10-"), 10).is_err());
        assert!(parse_range(Some("bytes=2-1"), 10).is_err());
        assert!(parse_range(Some("bytes=0-1,4-5"), 10).is_err());
    }

    #[tokio::test]
    async fn returns_partial_content_for_video_range() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"0123456789").unwrap();
        let headers = HeaderMap::from_iter([(RANGE, HeaderValue::from_static("bytes=2-5"))]);
        let response = serve_file(file.path().to_path_buf(), &headers, &Method::GET)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"2345");
    }
}
