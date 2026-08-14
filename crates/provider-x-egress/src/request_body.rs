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
