use std::io::{self, Read};

use encoding_rs::{Encoding, UTF_8};
use encoding_rs_io::DecodeReaderBytesBuilder;
use mime::Mime;
use reqwest::blocking::{Request, Response};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_LENGTH, CONTENT_TYPE, HOST};

use crate::{
    formatting::{get_json_formatter, Highlighter},
    utils::{copy_largebuf, get_content_type, test_mode, ContentType},
};
use crate::{Buffer, Pretty, Theme};

const MULTIPART_SUPPRESSOR: &str = concat!(
    "+--------------------------------------------+\n",
    "| NOTE: multipart data not shown in terminal |\n",
    "+--------------------------------------------+\n",
    "\n"
);

pub(crate) const BINARY_SUPPRESSOR: &str = concat!(
    "+-----------------------------------------+\n",
    "| NOTE: binary data not shown in terminal |\n",
    "+-----------------------------------------+\n",
    "\n"
);

pub struct Printer {
    indent_json: bool,
    color: bool,
    theme: Theme,
    sort_headers: bool,
    stream: bool,
    buffer: Buffer,
}

impl Printer {
    pub fn new(pretty: Option<Pretty>, theme: Option<Theme>, stream: bool, buffer: Buffer) -> Self {
        let pretty = pretty.unwrap_or_else(|| Pretty::from(&buffer));
        let theme = theme.unwrap_or(Theme::auto);

        Printer {
            indent_json: pretty.format(),
            sort_headers: pretty.format(),
            color: pretty.color(),
            stream,
            theme,
            buffer,
        }
    }

    /// Run a piece of code with a [`Highlighter`] instance. After the code runs
    /// successfully, [`Highlighter::finish`] will be called to properly terminate.
    ///
    /// That way you don't have to remember to call it manually, and errors
    /// can still be handled (unlike an implementation of [`Drop`]).
    fn with_highlighter(
        &mut self,
        syntax: &'static str,
        code: impl FnOnce(&mut Highlighter) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut highlighter = Highlighter::new(syntax, self.theme, &mut self.buffer);
        code(&mut highlighter)?;
        highlighter.finish()
    }

    fn print_colorized_text(&mut self, text: &str, syntax: &'static str) -> io::Result<()> {
        self.with_highlighter(syntax, |highlighter| highlighter.highlight(text))
    }

    fn print_stream(&mut self, reader: &mut impl Read) -> io::Result<()> {
        copy_largebuf(reader, &mut self.buffer)?;
        Ok(())
    }

    fn print_json_text(&mut self, text: &str) -> io::Result<()> {
        if !self.indent_json {
            // We don't have to do anything specialized, so fall back to the generic version
            self.print_syntax_text(text, "json")
        } else if self.color {
            let mut buf = Vec::new();
            get_json_formatter().format_stream_unbuffered(&mut text.as_bytes(), &mut buf)?;
            // in principle, buf should already be valid UTF-8,
            // because JSONXF doesn't mangle it
            let text = String::from_utf8_lossy(&buf);
            self.print_colorized_text(&text, "json")
        } else {
            get_json_formatter().format_stream_unbuffered(&mut text.as_bytes(), &mut self.buffer)
        }
    }

    fn print_json_stream(&mut self, stream: &mut impl Read) -> io::Result<()> {
        if !self.indent_json {
            // We don't have to do anything specialized, so fall back to the generic version
            self.print_syntax_stream(stream, "json")
        } else if self.color {
            self.with_highlighter("json", |highlighter| {
                get_json_formatter().format_stream_unbuffered(stream, &mut highlighter.linewise())
            })
        } else {
            get_json_formatter().format_stream_unbuffered(stream, &mut self.buffer)
        }
    }

    fn print_syntax_text(&mut self, text: &str, syntax: &'static str) -> io::Result<()> {
        if self.color {
            self.print_colorized_text(text, syntax)
        } else {
            self.buffer.print(text)
        }
    }

    fn print_syntax_stream(
        &mut self,
        stream: &mut impl Read,
        syntax: &'static str,
    ) -> io::Result<()> {
        if self.color {
            self.with_highlighter(syntax, |highlighter| {
                io::copy(stream, &mut highlighter.linewise())?;
                Ok(())
            })
        } else {
            self.print_stream(stream)
        }
    }

    fn print_headers(&mut self, text: &str) -> io::Result<()> {
        if self.color {
            self.print_colorized_text(text, "http")
        } else {
            self.buffer.print(text)
        }
    }

    fn headers_to_string(&self, headers: &HeaderMap, sort: bool) -> String {
        let mut headers: Vec<(&HeaderName, &HeaderValue)> = headers.iter().collect();
        if sort {
            headers.sort_by(|(a, _), (b, _)| a.to_string().cmp(&b.to_string()))
        }

        let mut header_string = String::new();
        for (key, value) in headers {
            header_string.push_str(key.as_str());
            header_string.push_str(": ");
            match value.to_str() {
                Ok(value) => header_string.push_str(value),
                Err(_) => header_string.push_str(&format!("{:?}", value)),
            }
            header_string.push('\n');
        }
        header_string.pop();

        header_string
    }

    pub fn print_request_headers(&mut self, request: &Request) -> io::Result<()> {
        let method = request.method();
        let url = request.url();
        let query_string = url.query().map_or(String::from(""), |q| ["?", q].concat());
        let version = reqwest::Version::HTTP_11;
        let mut headers = request.headers().clone();

        // See https://github.com/seanmonstar/reqwest/issues/1030
        // reqwest and hyper add certain headers, but only in the process of
        // sending the request, which we haven't done yet
        if let Some(body) = request.body().and_then(|body| body.as_bytes()) {
            // Added at https://github.com/seanmonstar/reqwest/blob/e56bd160ba/src/blocking/request.rs#L132
            headers
                .entry(CONTENT_LENGTH)
                .or_insert_with(|| body.len().into());
        }
        if let Some(host) = request.url().host_str() {
            // This is incorrect in case of HTTP/2, but we're already assuming
            // HTTP/1.1 anyway
            headers.entry(HOST).or_insert_with(|| {
                // Added at https://github.com/hyperium/hyper/blob/dfa1bb291d/src/client/client.rs#L237
                if test_mode() {
                    HeaderValue::from_str("http.mock")
                } else if let Some(port) = request.url().port() {
                    HeaderValue::from_str(&format!("{}:{}", host, port))
                } else {
                    HeaderValue::from_str(host)
                }
                .expect("hostname should already be validated/parsed")
            });
        }

        let request_line = format!("{} {}{} {:?}\n", method, url.path(), query_string, version);
        let headers = &self.headers_to_string(&headers, self.sort_headers);

        self.print_headers(&(request_line + &headers))?;
        self.buffer.print("\n\n")?;
        Ok(())
    }

    pub fn print_response_headers(&mut self, response: &Response) -> io::Result<()> {
        let version = response.version();
        let status = response.status();
        let headers = response.headers();

        let status_line = format!("{:?} {}\n", version, status);
        let headers = self.headers_to_string(headers, self.sort_headers);

        self.print_headers(&(status_line + &headers))?;
        self.buffer.print("\n\n")?;
        Ok(())
    }

    fn print_body_text(&mut self, content_type: Option<ContentType>, body: &str) -> io::Result<()> {
        match content_type {
            Some(ContentType::Json) => self.print_json_text(body),
            Some(ContentType::Xml) => self.print_syntax_text(body, "xml"),
            Some(ContentType::Html) => self.print_syntax_text(body, "html"),
            _ => self.buffer.print(body),
        }
    }

    fn print_body_stream(
        &mut self,
        content_type: Option<ContentType>,
        body: &mut impl Read,
    ) -> io::Result<()> {
        match content_type {
            Some(ContentType::Json) => self.print_json_stream(body),
            Some(ContentType::Xml) => self.print_syntax_stream(body, "xml"),
            Some(ContentType::Html) => self.print_syntax_stream(body, "html"),
            _ => self.print_stream(body),
        }
    }

    pub fn print_request_body(&mut self, request: &Request) -> io::Result<()> {
        match get_content_type(&request.headers()) {
            Some(ContentType::Multipart) => {
                self.buffer.print(MULTIPART_SUPPRESSOR)?;
            }
            // TODO: Should this print BINARY_SUPPRESSOR?
            content_type => {
                if let Some(body) = request
                    .body()
                    .and_then(|b| b.as_bytes())
                    .filter(|b| !b.contains(&b'\0'))
                    .and_then(|b| String::from_utf8(b.into()).ok())
                {
                    self.print_body_text(content_type, &body)?;
                    self.buffer.print("\n")?;
                }
            }
        }
        Ok(())
    }

    pub fn print_response_body(&mut self, mut response: Response) -> anyhow::Result<()> {
        if !self.buffer.is_terminal() {
            // No trailing newlines, no binary detection, no decoding, direct streaming
            self.print_body_stream(get_content_type(&response.headers()), &mut response)?;
        } else if self.stream {
            self.print_body_stream(
                get_content_type(&response.headers()),
                &mut decode_stream(&mut response),
            )?;
            self.buffer.print("\n")?;
        } else {
            let content_type = get_content_type(&response.headers());
            // Note that .text() behaves like String::from_utf8_lossy()
            let text = response.text()?;
            if text.contains('\0') {
                self.buffer.print(BINARY_SUPPRESSOR)?;
                return Ok(());
            }
            self.print_body_text(content_type, &text)?;
            self.buffer.print("\n")?;
        }
        Ok(())
    }
}

/// Decode a streaming response in a way that matches `.text()`.
///
/// Note that in practice this seems to behave like String::from_utf8_lossy(),
/// but it makes no guarantees about outputting valid UTF-8 if the input is
/// invalid UTF-8 (claiming to be UTF-8). So only pass data through here
/// that's going to the terminal, and don't trust its output.
///
/// `reqwest` doesn't provide an API for this, so we have to roll our own. It
/// doesn't even provide an API to detect the response's encoding, so that
/// logic is copied here.
///
/// See https://github.com/seanmonstar/reqwest/blob/2940740493/src/async_impl/response.rs#L172
fn decode_stream(response: &mut Response) -> impl Read + '_ {
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Mime>().ok());
    let encoding_name = content_type
        .as_ref()
        .and_then(|mime| mime.get_param("charset").map(|charset| charset.as_str()))
        .unwrap_or("utf-8");
    let encoding = Encoding::for_label(encoding_name.as_bytes()).unwrap_or(UTF_8);

    DecodeReaderBytesBuilder::new()
        .encoding(Some(encoding))
        .build(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{buffer::BufferKind, cli::Cli, vec_of_strings};
    use assert_matches::assert_matches;

    fn run_cmd(args: impl IntoIterator<Item = String>, is_stdout_tty: bool) -> Printer {
        let args = Cli::from_iter_safe(args).unwrap();
        let buffer = Buffer::new(args.download, &args.output, is_stdout_tty).unwrap();
        Printer::new(args.pretty, args.theme, false, buffer)
    }

    fn temp_path(filename: &str) -> String {
        let mut dir = std::env::temp_dir();
        dir.push(filename);
        dir.to_str().unwrap().to_owned()
    }

    #[test]
    fn test_1() {
        let p = run_cmd(vec_of_strings!["xh", "httpbin.org/get"], true);
        assert_eq!(p.color, true);
        assert_matches!(p.buffer.kind, BufferKind::Stdout);
    }

    #[test]
    fn test_2() {
        let p = run_cmd(vec_of_strings!["xh", "httpbin.org/get"], false);
        assert_eq!(p.color, false);
        assert_matches!(p.buffer.kind, BufferKind::Redirect);
    }

    #[test]
    fn test_3() {
        let output = temp_path("temp3");
        let p = run_cmd(vec_of_strings!["xh", "httpbin.org/get", "-o", output], true);
        assert_eq!(p.color, false);
        assert_matches!(p.buffer.kind, BufferKind::File(_));
    }

    #[test]
    fn test_4() {
        let output = temp_path("temp4");
        let p = run_cmd(
            vec_of_strings!["xh", "httpbin.org/get", "-o", output],
            false,
        );
        assert_eq!(p.color, false);
        assert_matches!(p.buffer.kind, BufferKind::File(_));
    }

    #[test]
    fn test_5() {
        let p = run_cmd(vec_of_strings!["xh", "httpbin.org/get", "-d"], true);
        assert_eq!(p.color, true);
        assert_matches!(p.buffer.kind, BufferKind::Stderr);
    }

    #[test]
    fn test_6() {
        let p = run_cmd(vec_of_strings!["xh", "httpbin.org/get", "-d"], false);
        assert_eq!(p.color, true);
        assert_matches!(p.buffer.kind, BufferKind::Stderr);
    }

    #[test]
    fn test_7() {
        let output = temp_path("temp7");
        let p = run_cmd(
            vec_of_strings!["xh", "httpbin.org/get", "-d", "-o", output],
            true,
        );
        assert_eq!(p.color, true);
        assert_matches!(p.buffer.kind, BufferKind::Stderr);
    }

    #[test]
    fn test_8() {
        let output = temp_path("temp8");
        let p = run_cmd(
            vec_of_strings!["xh", "httpbin.org/get", "-d", "-o", output],
            false,
        );
        assert_eq!(p.color, true);
        assert_matches!(p.buffer.kind, BufferKind::Stderr);
    }
}
