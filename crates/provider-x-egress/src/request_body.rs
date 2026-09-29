use std::io::{Cursor, Read};

use bytes::Bytes;
use hyper::{HeaderMap, header};

use crate::ProxyError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestEncoding {
    Identity,
    Zstd,
}

impl RequestEncoding {
    pub(crate) fn from_headers(headers: &HeaderMap) -> Result<Self, ProxyError> {
        let mut values = headers.get_all(header::CONTENT_ENCODING).iter();
        let Some(value) = values.next() else {
            return Ok(Self::Identity);
        };
        if values.next().is_some() {
            return Err(ProxyError::UnsupportedContentEncoding);
        }
        let value = value
            .to_str()
            .map_err(|_| ProxyError::UnsupportedContentEncoding)?
            .trim();
        if value.eq_ignore_ascii_case("identity") {
            Ok(Self::Identity)
        } else if value.eq_ignore_ascii_case("zstd") {
            Ok(Self::Zstd)
        } else {
            Err(ProxyError::UnsupportedContentEncoding)
        }
    }

    pub(crate) async fn decode(self, body: Bytes, limit: usize) -> Result<Bytes, ProxyError> {
        match self {
            Self::Identity => Ok(body),
            Self::Zstd => tokio::task::spawn_blocking(move || decode_zstd(&body, limit))
                .await
                .map_err(|_| ProxyError::InvalidRequest("zstd decoder task failed".to_owned()))?,
        }
    }
}

fn decode_zstd(body: &[u8], limit: usize) -> Result<Bytes, ProxyError> {
    let decoder = zstd::stream::read::Decoder::new(Cursor::new(body))
        .map_err(|_| ProxyError::InvalidRequest("invalid zstd request body".to_owned()))?;
    let read_limit = u64::try_from(limit)
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1);
    let mut limited = decoder.take(read_limit);
    let mut output = Vec::with_capacity(limit.min(body.len().saturating_mul(4)));
    limited
        .read_to_end(&mut output)
        .map_err(|_| ProxyError::InvalidRequest("invalid zstd request body".to_owned()))?;
    if output.len() > limit {
        return Err(ProxyError::BodyTooLarge);
    }
    Ok(Bytes::from(output))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use hyper::{HeaderMap, header::HeaderValue};

    use super::RequestEncoding;
    use crate::ProxyError;

    // Generated independently with the Zstandard 1.5.7 CLI, without a checksum.
    const ZSTD_FRAME: &[u8] =
        b"\x28\xb5\x2f\xfd\x00\x58\x21\x01\x00{\"model\":\"gpt-test\",\"input\":\"hello\"}";
    const ZSTD_BODY: &[u8] = br#"{"model":"gpt-test","input":"hello"}"#;

    #[tokio::test]
    async fn zstd_decodes_an_independent_frame_and_bounds_concatenated_frames() {
        assert_eq!(
            RequestEncoding::Zstd
                .decode(Bytes::from_static(ZSTD_FRAME), ZSTD_BODY.len())
                .await
                .unwrap(),
            ZSTD_BODY
        );
        let frames = Bytes::from([ZSTD_FRAME, ZSTD_FRAME].concat());
        assert_eq!(
            RequestEncoding::Zstd
                .decode(frames.clone(), ZSTD_BODY.len() * 2)
                .await
                .unwrap(),
            [ZSTD_BODY, ZSTD_BODY].concat()
        );
        assert!(matches!(
            RequestEncoding::Zstd.decode(frames, ZSTD_BODY.len()).await,
            Err(ProxyError::BodyTooLarge)
        ));
    }

    #[tokio::test]
    async fn zstd_rejects_truncated_frames_and_invalid_trailing_data() {
        for invalid in [
            ZSTD_FRAME[..ZSTD_FRAME.len() - 1].to_vec(),
            [ZSTD_FRAME, b"invalid"].concat(),
        ] {
            assert!(matches!(
                RequestEncoding::Zstd
                    .decode(Bytes::from(invalid), ZSTD_BODY.len() + 1)
                    .await,
                Err(ProxyError::InvalidRequest(_))
            ));
        }
    }

    #[tokio::test]
    async fn zstd_decode_is_bounded() {
        let original = Bytes::from_static(br#"{"model":"gpt-5.6","input":"hello"}"#);
        let encoded = Bytes::from(
            zstd::stream::encode_all(std::io::Cursor::new(original.clone()), 0).unwrap(),
        );
        assert_ne!(encoded, original);
        assert_eq!(
            RequestEncoding::Zstd
                .decode(encoded.clone(), original.len())
                .await
                .unwrap(),
            original
        );
        assert!(matches!(
            RequestEncoding::Zstd
                .decode(encoded, original.len() - 1)
                .await,
            Err(ProxyError::BodyTooLarge)
        ));
    }

    #[test]
    fn only_identity_and_zstd_are_accepted() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            RequestEncoding::from_headers(&headers).unwrap(),
            RequestEncoding::Identity
        );
        headers.insert("content-encoding", HeaderValue::from_static("zstd"));
        assert_eq!(
            RequestEncoding::from_headers(&headers).unwrap(),
            RequestEncoding::Zstd
        );
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));
        assert!(matches!(
            RequestEncoding::from_headers(&headers),
            Err(ProxyError::UnsupportedContentEncoding)
        ));
    }
}
