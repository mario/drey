//! LSP base protocol framing: `Content-Length` headers over a byte stream.

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Reads one framed message body. `Ok(None)` means the peer closed.
pub async fn read_message<R: AsyncRead + Unpin>(r: &mut BufReader<R>) -> Result<Option<Vec<u8>>> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        // Header names are case-insensitive. Matching two spellings and no
        // others was arbitrary: `CONTENT-LENGTH:` fell through and the message
        // was rejected as headerless, which is a confusing way to say
        // "unexpected capitalisation".
        if let Some(v) = split_header(line, "content-length") {
            let parsed: usize = v.trim().parse().context("bad Content-Length")?;
            // Two headers means the sender and we disagree about where this
            // message ends, and letting the last one win desynchronises the
            // stream silently: every message after it is read at the wrong
            // offset. Refuse instead, while the error still points at a cause.
            if let Some(first) = len {
                bail!("two Content-Length headers in one message ({first} then {parsed})");
            }
            len = Some(parsed);
        }
    }
    let Some(len) = len else {
        bail!("LSP message without Content-Length header");
    };
    // A wedged or hostile peer should not be able to make us allocate the heap.
    if len > 512 * 1024 * 1024 {
        bail!("LSP message of {len} bytes exceeds the 512 MiB sanity limit");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

/// Value of `name` if `line` is that header, matched without regard to case.
/// `name` must already be lowercase.
fn split_header<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let (field, value) = line.split_once(':')?;
    field.trim().eq_ignore_ascii_case(name).then_some(value)
}

pub async fn write_message<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> Result<()> {
    w.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    w.write_all(body).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips() {
        let mut buf = Vec::new();
        write_message(&mut buf, b"{\"a\":1}").await.unwrap();
        assert_eq!(buf, b"Content-Length: 7\r\n\r\n{\"a\":1}");

        let mut r = BufReader::new(&buf[..]);
        assert_eq!(read_message(&mut r).await.unwrap().unwrap(), b"{\"a\":1}");
        assert!(read_message(&mut r).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn tolerates_extra_headers_and_any_casing() {
        // Header names are case-insensitive, so all three spellings are the
        // same header. Accepting two of them and not the third was arbitrary.
        for name in ["Content-Length", "content-length", "CONTENT-LENGTH"] {
            let raw = format!("{name}: 2\r\nContent-Type: application/vscode-jsonrpc\r\n\r\n{{}}");
            let mut r = BufReader::new(raw.as_bytes());
            assert_eq!(
                read_message(&mut r).await.unwrap().unwrap(),
                b"{}",
                "failed for {name}"
            );
        }
    }

    #[tokio::test]
    async fn a_header_whose_name_merely_ends_in_content_length_is_not_one() {
        // `split_once(':')` plus an exact name match, so a lookalike field is
        // ignored rather than parsed as the length.
        let raw = b"X-Content-Length: 999\r\nContent-Length: 2\r\n\r\n{}";
        let mut r = BufReader::new(&raw[..]);
        assert_eq!(read_message(&mut r).await.unwrap().unwrap(), b"{}");
    }

    #[tokio::test]
    async fn rejects_missing_length() {
        let raw = b"X-Nonsense: 1\r\n\r\n{}";
        let mut r = BufReader::new(&raw[..]);
        assert!(read_message(&mut r).await.is_err());
    }

    /// Yields one byte per poll, so a reader that assumed a whole header or
    /// body arrives in a single read fails here.
    struct Dribble(Vec<u8>, usize);

    impl AsyncRead for Dribble {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if self.1 < self.0.len() {
                let b = self.0[self.1];
                self.1 += 1;
                buf.put_slice(&[b]);
            }
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn reassembles_a_message_split_across_reads() {
        let mut wire = Vec::new();
        write_message(&mut wire, b"{\"method\":\"one\"}")
            .await
            .unwrap();
        write_message(&mut wire, b"{\"method\":\"two\"}")
            .await
            .unwrap();

        let mut r = BufReader::new(Dribble(wire, 0));
        assert_eq!(
            read_message(&mut r).await.unwrap().unwrap(),
            b"{\"method\":\"one\"}"
        );
        assert_eq!(
            read_message(&mut r).await.unwrap().unwrap(),
            b"{\"method\":\"two\"}"
        );
        assert!(read_message(&mut r).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reads_two_messages_from_one_buffer() {
        let mut wire = Vec::new();
        write_message(&mut wire, b"a").await.unwrap();
        write_message(&mut wire, b"bb").await.unwrap();
        let mut r = BufReader::new(&wire[..]);
        assert_eq!(read_message(&mut r).await.unwrap().unwrap(), b"a");
        assert_eq!(read_message(&mut r).await.unwrap().unwrap(), b"bb");
    }

    #[tokio::test]
    async fn a_clean_close_before_any_header_is_not_an_error() {
        let mut r = BufReader::new(&b""[..]);
        assert!(read_message(&mut r).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_truncated_body_is_an_error_not_a_short_message() {
        let raw = b"Content-Length: 10\r\n\r\nshort";
        let mut r = BufReader::new(&raw[..]);
        assert!(read_message(&mut r).await.is_err());
    }

    #[tokio::test]
    async fn rejects_a_non_numeric_length() {
        for raw in [
            &b"Content-Length: nope\r\n\r\n{}"[..],
            &b"Content-Length: -1\r\n\r\n{}"[..],
            &b"Content-Length: 1 2\r\n\r\n{}"[..],
        ] {
            let mut r = BufReader::new(raw);
            assert!(read_message(&mut r).await.is_err(), "{:?}", raw);
        }
    }

    #[tokio::test]
    async fn refuses_to_allocate_for_an_absurd_length() {
        // 1 GiB claimed, nothing behind it: the guard must fire before we try
        // to reserve the buffer.
        let raw = b"Content-Length: 1073741824\r\n\r\n";
        let mut r = BufReader::new(&raw[..]);
        let err = read_message(&mut r).await.unwrap_err().to_string();
        assert!(err.contains("512 MiB"), "{err}");
    }

    #[tokio::test]
    async fn an_empty_body_is_a_message_not_a_close() {
        let raw = b"Content-Length: 0\r\n\r\n";
        let mut r = BufReader::new(&raw[..]);
        assert_eq!(read_message(&mut r).await.unwrap().unwrap(), b"");
    }

    #[tokio::test]
    async fn tolerates_bare_lf_line_endings() {
        let raw = b"Content-Length: 2\n\n{}";
        let mut r = BufReader::new(&raw[..]);
        assert_eq!(read_message(&mut r).await.unwrap().unwrap(), b"{}");
    }

    #[tokio::test]
    async fn two_content_length_headers_are_refused() {
        // This used to let the last one win. A peer that sends two disagrees
        // with us about where the message ends, so every message after it is
        // read at the wrong offset: a silent desync rather than a loud error.
        let raw = b"Content-Length: 99\r\nContent-Length: 2\r\n\r\n{}";
        let mut r = BufReader::new(&raw[..]);
        let err = read_message(&mut r).await.unwrap_err().to_string();
        assert!(err.contains("two Content-Length headers"), "{err}");
    }

    #[tokio::test]
    async fn bodies_are_bytes_not_text() {
        // The length is in bytes, so multi-byte UTF-8 must not be miscounted.
        let body = "{\"s\":\"héllo 🦀\"}".as_bytes().to_vec();
        let mut wire = Vec::new();
        write_message(&mut wire, &body).await.unwrap();
        assert!(wire.starts_with(format!("Content-Length: {}\r\n", body.len()).as_bytes()));
        let mut r = BufReader::new(&wire[..]);
        assert_eq!(read_message(&mut r).await.unwrap().unwrap(), body);
    }

    proptest::proptest! {
        #[test]
        fn any_body_survives_a_write_read_round_trip(body: Vec<u8>) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let mut wire = Vec::new();
                write_message(&mut wire, &body).await.unwrap();
                let mut r = BufReader::new(&wire[..]);
                proptest::prop_assert_eq!(read_message(&mut r).await.unwrap(), Some(body.clone()));
                proptest::prop_assert_eq!(read_message(&mut r).await.unwrap(), None);
                Ok(())
            })?;
        }
    }
}
