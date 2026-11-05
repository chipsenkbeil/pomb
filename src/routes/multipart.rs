use std::fmt;
use std::pin::Pin;

use axum::body::Body;
use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use http_body::Body as HttpBody;

/// Bytes the headers of one part may take before the request is refused.
const MAX_HEAD_BYTES: usize = 8 * 1024;

/// Bytes a boundary may take.
const MAX_BOUNDARY_BYTES: usize = 200;

/// Why a multipart body could not be read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MultipartError {
    /// The request has no multipart content type or no boundary.
    NoBoundary,
    /// The boundary is longer than [`MAX_BOUNDARY_BYTES`].
    LongBoundary,
    /// The body ended or broke the format before the closing boundary.
    Malformed(&'static str),
    /// The connection broke while the body was being read.
    Interrupted,
}

impl fmt::Display for MultipartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoBoundary => {
                f.write_str("a multipart/form-data upload needs a boundary in its content type")
            }
            Self::LongBoundary => write!(
                f,
                "a multipart/form-data boundary takes at most {MAX_BOUNDARY_BYTES} bytes"
            ),
            Self::Malformed(why) => write!(f, "the multipart body is malformed: {why}"),
            Self::Interrupted => f.write_str("the upload was interrupted"),
        }
    }
}

/// The headers of one part that matter to an upload.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct PartHead {
    /// The file name the client gave, which only a file part has.
    pub(crate) filename: Option<String>,
    /// The MIME type the client gave.
    pub(crate) content_type: Option<String>,
}

/// Where the reader stands in the body.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum State {
    /// Before the first boundary.
    Start,
    /// Just after a boundary, where `--` or a line break follows.
    Between,
    /// Inside the headers of a part.
    Headers,
    /// Inside the data of a part.
    Data,
    /// After the closing boundary.
    Done,
}

/// What one look at the buffer between parts came to.
enum Step {
    /// The reader moved on.
    Moved,
    /// The buffer holds too little to tell.
    NeedBytes,
    /// The headers of the next part are read.
    Part(PartHead),
}

/// A streaming reader of a multipart/form-data body, which holds at most one chunk and a boundary
/// in memory at a time, so a part of any size goes to disk as it arrives.
pub(crate) struct Multipart {
    /// The request body.
    body: Body,
    /// The line break, dashes and boundary that end a part's data.
    delimiter: Vec<u8>,
    /// Bytes read and not yet handed out.
    buf: Vec<u8>,
    /// Whether the body has no more bytes.
    ended: bool,
    /// Where the reader stands.
    state: State,
}

/// Returns the boundary of the multipart content type in `headers`.
pub(crate) fn boundary_of(headers: &HeaderMap) -> Result<String, MultipartError> {
    let value = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .ok_or(MultipartError::NoBoundary)?;
    let mut params = value.split(';');
    let kind = params.next().unwrap_or_default().trim();
    if !kind.eq_ignore_ascii_case("multipart/form-data") {
        return Err(MultipartError::NoBoundary);
    }
    let boundary = params
        .filter_map(|param| param.split_once('='))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case("boundary"))
        .map(|(_, boundary)| boundary.trim().trim_matches('"').to_string())
        .filter(|boundary| !boundary.is_empty())
        .ok_or(MultipartError::NoBoundary)?;
    if boundary.len() > MAX_BOUNDARY_BYTES {
        return Err(MultipartError::LongBoundary);
    }
    Ok(boundary)
}

/// Returns whether `headers` announce a multipart/form-data body.
pub(crate) fn is_multipart(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("multipart/form-data")
        })
}

/// Returns where `needle` first occurs in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Returns the value of the parameter `key` in a header such as `form-data; name="a"`.
fn param(header: &str, key: &str) -> Option<String> {
    let mut rest = header;
    while let Some(at) = rest.find(key) {
        let before = rest[..at].chars().next_back();
        let after = &rest[at + key.len()..];
        if matches!(before, Some(';' | ' ' | '\t')) && after.starts_with('=') {
            let value = &after[1..];
            return Some(match value.strip_prefix('"') {
                Some(quoted) => quoted.split('"').next().unwrap_or_default().to_string(),
                None => value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            });
        }
        rest = after;
    }
    None
}

impl Multipart {
    /// Reads `body`, whose parts are separated by `boundary`.
    pub(crate) fn new(body: Body, boundary: &str) -> Self {
        let mut delimiter = b"\r\n--".to_vec();
        delimiter.extend_from_slice(boundary.as_bytes());
        Self {
            body,
            delimiter,
            buf: b"\r\n".to_vec(),
            ended: false,
            state: State::Start,
        }
    }

    /// Reads the next piece of the body into the buffer, or notes the end.
    async fn fill(&mut self) -> Result<(), MultipartError> {
        loop {
            let frame = std::future::poll_fn(|cx| Pin::new(&mut self.body).poll_frame(cx)).await;
            match frame {
                None => {
                    self.ended = true;
                    return Ok(());
                }
                Some(Err(_)) => return Err(MultipartError::Interrupted),
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        self.buf.extend_from_slice(&data);
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Moves to the headers of the next part and returns them, or [`None`] after the last part.
    /// The rest of the part the reader is in is skipped.
    pub(crate) async fn next_part(&mut self) -> Result<Option<PartHead>, MultipartError> {
        loop {
            let step = match self.state {
                State::Done => return Ok(None),
                State::Data => {
                    self.next_chunk().await?;
                    Step::Moved
                }
                State::Start => self.skip_preamble()?,
                State::Between => self.leave_boundary()?,
                State::Headers => self.read_head()?,
            };
            match step {
                Step::Moved => {}
                Step::NeedBytes => self.fill().await?,
                Step::Part(head) => return Ok(Some(head)),
            }
        }
    }

    /// Asks for more bytes, or fails with `why` when the body has none left.
    fn more(&self, why: &'static str) -> Result<Step, MultipartError> {
        if self.ended {
            return Err(MultipartError::Malformed(why));
        }
        Ok(Step::NeedBytes)
    }

    /// Drops what comes before the first boundary.
    fn skip_preamble(&mut self) -> Result<Step, MultipartError> {
        if let Some(at) = find(&self.buf, &self.delimiter) {
            self.buf.drain(..at + self.delimiter.len());
            self.state = State::Between;
            return Ok(Step::Moved);
        }
        let keep = self.delimiter.len().saturating_sub(1);
        let drop = self.buf.len().saturating_sub(keep);
        self.buf.drain(..drop);
        self.more("no boundary found")
    }

    /// Reads what follows a boundary: the closing dashes, or the line break before the headers.
    fn leave_boundary(&mut self) -> Result<Step, MultipartError> {
        if self.buf.len() < 2 {
            return self.more("no closing boundary");
        }
        if self.buf.starts_with(b"--") {
            self.state = State::Done;
        } else if self.buf.starts_with(b"\r\n") {
            self.buf.drain(..2);
            self.state = State::Headers;
        } else {
            return Err(MultipartError::Malformed("text after a boundary"));
        }
        Ok(Step::Moved)
    }

    /// Reads the headers of a part once the buffer holds them.
    fn read_head(&mut self) -> Result<Step, MultipartError> {
        let head = if self.buf.starts_with(b"\r\n") {
            self.buf.drain(..2);
            PartHead::default()
        } else if let Some(at) = find(&self.buf, b"\r\n\r\n") {
            let head = parse_head(&self.buf[..at])?;
            self.buf.drain(..at + 4);
            head
        } else if self.buf.len() > MAX_HEAD_BYTES {
            return Err(MultipartError::Malformed("a part's headers are too long"));
        } else {
            return self.more("a part's headers are cut off");
        };
        self.state = State::Data;
        Ok(Step::Part(head))
    }

    /// Returns the next bytes of the current part's data, or [`None`] at its end.
    pub(crate) async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, MultipartError> {
        if self.state != State::Data {
            return Ok(None);
        }
        loop {
            if let Some(at) = find(&self.buf, &self.delimiter) {
                if at > 0 {
                    return Ok(Some(self.buf.drain(..at).collect()));
                }
                let after = &self.buf[self.delimiter.len()..];
                if after.len() < 2 && !self.ended {
                    self.fill().await?;
                    continue;
                }
                if after.len() >= 2 && !after.starts_with(b"--") && !after.starts_with(b"\r\n") {
                    return Ok(Some(self.buf.drain(..1).collect()));
                }
                self.buf.drain(..self.delimiter.len());
                self.state = State::Between;
                return Ok(None);
            }
            let keep = self.delimiter.len().saturating_sub(1);
            if self.buf.len() > keep {
                let send = self.buf.len() - keep;
                return Ok(Some(self.buf.drain(..send).collect()));
            }
            if self.ended {
                return Err(MultipartError::Malformed("a part is cut off"));
            }
            self.fill().await?;
        }
    }
}

/// Reads the headers of a part from their bytes.
fn parse_head(bytes: &[u8]) -> Result<PartHead, MultipartError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| MultipartError::Malformed("a part's headers are not UTF-8"))?;
    let mut head = PartHead::default();
    for line in text.split("\r\n") {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if key.trim().eq_ignore_ascii_case("content-disposition") {
            head.filename = param(value, "filename");
        } else if key.trim().eq_ignore_ascii_case("content-type") {
            head.content_type = Some(value.to_string());
        }
    }
    Ok(head)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::convert::Infallible;
    use std::task::{Context, Poll};

    use axum::body::{Body, Bytes};
    use axum::http::HeaderValue;
    use http_body::Frame;

    use super::*;

    const BOUNDARY: &str = "XyZ";

    struct Chunked(VecDeque<Bytes>);

    impl HttpBody for Chunked {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
            Poll::Ready(self.0.pop_front().map(|chunk| Ok(Frame::data(chunk))))
        }
    }

    fn chunked(chunks: Vec<Vec<u8>>) -> Body {
        Body::new(Chunked(chunks.into_iter().map(Bytes::from).collect()))
    }

    async fn read_all(body: Vec<Vec<u8>>) -> Result<Vec<(PartHead, Vec<u8>)>, MultipartError> {
        let mut reader = Multipart::new(chunked(body), BOUNDARY);
        let mut parts = Vec::new();
        while let Some(head) = reader.next_part().await? {
            let mut data = Vec::new();
            while let Some(chunk) = reader.next_chunk().await? {
                data.extend_from_slice(&chunk);
            }
            parts.push((head, data));
        }
        Ok(parts)
    }

    fn file_part(name: &str, data: &[u8]) -> Vec<u8> {
        let mut part = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
             Content-Type: text/plain\r\n\r\n"
        )
        .into_bytes();
        part.extend_from_slice(data);
        part.extend_from_slice(b"\r\n");
        part
    }

    fn closed(mut parts: Vec<u8>) -> Vec<u8> {
        parts.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        parts
    }

    #[tokio::test]
    async fn next_part_should_read_every_file_with_its_headers() {
        let mut body = file_part("a.txt", b"one");
        body.extend(file_part("b.txt", b"two\r\nlines"));
        let parts = read_all(vec![closed(body)]).await.expect("parts");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].0.filename.as_deref(), Some("a.txt"));
        assert_eq!(parts[0].0.content_type.as_deref(), Some("text/plain"));
        assert_eq!(parts[0].1, b"one");
        assert_eq!(parts[1].0.filename.as_deref(), Some("b.txt"));
        assert_eq!(parts[1].1, b"two\r\nlines");
    }

    #[tokio::test]
    async fn next_part_should_read_a_body_cut_into_single_bytes() {
        let mut body = file_part("a.txt", b"one");
        body.extend(file_part("b.txt", b"--XyZ almost"));
        let whole = closed(body);
        let pieces = whole.iter().map(|byte| vec![*byte]).collect();
        let parts = read_all(pieces).await.expect("parts");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].1, b"one");
        assert_eq!(parts[1].1, b"--XyZ almost");
    }

    #[tokio::test]
    async fn next_part_should_skip_the_rest_of_a_part_nobody_read() {
        let mut body = file_part("a.txt", &vec![9u8; 200_000]);
        body.extend(file_part("b.txt", b"two"));
        let mut reader = Multipart::new(chunked(vec![closed(body)]), BOUNDARY);
        let first = reader.next_part().await.expect("part").expect("first");
        assert_eq!(first.filename.as_deref(), Some("a.txt"));
        let second = reader.next_part().await.expect("part").expect("second");
        assert_eq!(second.filename.as_deref(), Some("b.txt"));
        assert_eq!(
            reader.next_chunk().await.expect("chunk").as_deref(),
            Some(&b"two"[..])
        );
        assert!(reader.next_chunk().await.expect("end").is_none());
        assert!(reader.next_part().await.expect("done").is_none());
    }

    #[tokio::test]
    async fn next_part_should_refuse_a_body_cut_off_inside_a_part() {
        let mut body = file_part("a.txt", b"one");
        body.truncate(body.len() - 6);
        let error = read_all(vec![body]).await.expect_err("cut off");
        assert!(matches!(error, MultipartError::Malformed(_)), "{error:?}");
    }

    #[tokio::test]
    async fn next_part_should_refuse_a_body_cut_off_just_after_a_delimiter() {
        for tail in ["", "-", "\r"] {
            let mut body = file_part("a.txt", b"one");
            body.extend_from_slice(format!("--{BOUNDARY}{tail}").as_bytes());
            let error = read_all(vec![body]).await.expect_err("cut off");
            assert!(matches!(error, MultipartError::Malformed(_)), "{error:?}");
        }
    }

    #[tokio::test]
    async fn next_part_should_keep_data_that_only_starts_like_the_delimiter() {
        const DATA: &[u8] = b"a\r\n--Xy\r\n-\r\n--XY\r\n--XyZfoo\r\n--X";
        let parts = read_all(vec![closed(file_part("a.txt", DATA))])
            .await
            .expect("parts");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].1, DATA);
    }

    #[tokio::test]
    async fn next_part_should_read_an_empty_file_between_two_others() {
        let mut body = file_part("a.txt", b"one");
        body.extend(file_part("empty.txt", b""));
        body.extend(file_part("b.txt", b"two"));
        let parts = read_all(vec![closed(body)]).await.expect("parts");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1].0.filename.as_deref(), Some("empty.txt"));
        assert_eq!(parts[1].1, b"");
        assert_eq!(parts[2].1, b"two");
    }

    #[tokio::test]
    async fn next_part_should_read_a_part_with_no_headers() {
        let body = closed(format!("--{BOUNDARY}\r\n\r\ndata\r\n").into_bytes());
        let parts = read_all(vec![body]).await.expect("parts");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].0, PartHead::default());
        assert_eq!(parts[0].1, b"data");
    }

    #[tokio::test]
    async fn next_part_should_skip_a_preamble_and_an_epilogue() {
        let mut body = b"ignored preamble\r\n".to_vec();
        body.extend(file_part("a.txt", b"one"));
        let mut body = closed(body);
        body.extend_from_slice(format!("--{BOUNDARY}\r\nignored epilogue").as_bytes());
        let parts = read_all(vec![body]).await.expect("parts");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].0.filename.as_deref(), Some("a.txt"));
        assert_eq!(parts[0].1, b"one");
    }

    #[tokio::test]
    async fn next_part_should_read_the_same_parts_wherever_the_body_is_split() {
        let mut body = b"preamble\r\n".to_vec();
        body.extend(file_part("a.txt", b"one\r\n--Xy\r\n--XyZ-x"));
        body.extend(file_part("empty.txt", b""));
        let mut body = closed(body);
        body.extend_from_slice(b"epilogue");
        let whole = read_all(vec![body.clone()]).await.expect("parts");
        assert_eq!(whole.len(), 2);
        for at in 0..=body.len() {
            let (head, tail) = body.split_at(at);
            let split = read_all(vec![head.to_vec(), tail.to_vec()])
                .await
                .expect("parts");
            assert_eq!(split, whole, "split at byte {at}");
        }
    }

    #[test]
    fn boundary_of_should_refuse_a_boundary_over_its_limit() {
        let with = |length: usize| {
            let value = format!("multipart/form-data; boundary={}", "b".repeat(length));
            let mut headers = HeaderMap::new();
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_str(&value).expect("header value"),
            );
            boundary_of(&headers)
        };
        assert_eq!(
            with(MAX_BOUNDARY_BYTES).map(|b| b.len()),
            Ok(MAX_BOUNDARY_BYTES)
        );
        assert_eq!(
            with(MAX_BOUNDARY_BYTES + 1),
            Err(MultipartError::LongBoundary)
        );
    }

    #[test]
    fn boundary_of_should_read_a_quoted_or_plain_boundary_and_refuse_other_types() {
        let with = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static(value));
            headers
        };
        assert_eq!(
            boundary_of(&with("multipart/form-data; boundary=abc")).as_deref(),
            Ok("abc")
        );
        assert_eq!(
            boundary_of(&with("Multipart/Form-Data; charset=x; boundary=\"a b\"")).as_deref(),
            Ok("a b")
        );
        assert_eq!(
            boundary_of(&with("multipart/form-data")),
            Err(MultipartError::NoBoundary)
        );
        assert_eq!(
            boundary_of(&with("application/json; boundary=abc")),
            Err(MultipartError::NoBoundary)
        );
    }
}
