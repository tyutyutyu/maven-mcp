//! The size boundary of every `tools/call` result.
//!
//! Tool handlers return [`Json`], which serializes the structured result exactly like rmcp's
//! wrapper (`structuredContent` plus the same JSON text in `content`), but first counts the
//! serialized size without buffering and fails when it exceeds the result budget. The server
//! then measures the complete serialized tool result; a result above `MAX_MCP_RESPONSE_BYTES`
//! is stored and replaced by a [`DeferredResultMarker`], and `read_tool_result_page` returns
//! the stored JSON text in [`ToolResultPage`]s that each fit into the same limit.

use std::{
    io,
    sync::atomic::{AtomicUsize, Ordering},
};

use rmcp::{
    ErrorData,
    handler::server::tool::IntoCallToolResult,
    model::{CallToolResponse, CallToolResult, JsonObject},
};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

use super::result_store::{STORED_RESULT_TTL, longest_result_id};

/// The paging tool named by every deferred result marker.
pub(super) const READ_PAGE_TOOL: &str = "read_tool_result_page";
/// The marker property added to every deferrable output schema.
const MARKER_PROPERTY: &str = "deferred_result";
/// The widest character a stored JSON text can contain, measured as a page fragment.
const WIDEST_FRAGMENT_CHARACTER: char = '\u{10FFFF}';

const DEFERRED_INSTRUCTION: &str = "This tool result is larger than one MCP response may be (MAX_MCP_RESPONSE_BYTES), so it was stored in this server process instead of being returned. Call read_tool_result_page with the same project_path and this result_id and no cursor, then call it again with each returned next_cursor until complete is true. Concatenating every fragment in order yields the original structured JSON of this tool exactly. The result_id is valid only in this server process and expires 300 seconds after this response; older results may be evicted earlier.";

/// Serialization budget of one tool result; set from the server configuration.
static RESULT_SERIALIZATION_LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);

pub(super) fn set_result_serialization_limit(bytes: usize) {
    RESULT_SERIALIZATION_LIMIT.store(bytes, Ordering::Relaxed);
}

/// Structured tool output; the `tool` macro derives the output schema from `T`.
pub struct Json<T>(pub T);

impl<T: Serialize + 'static> IntoCallToolResult for Json<T> {
    fn into_call_tool_result(self) -> Result<CallToolResponse, ErrorData> {
        structured_result(&self.0, RESULT_SERIALIZATION_LIMIT.load(Ordering::Relaxed))
            .map(Into::into)
    }
}

/// rmcp's structured tool result of `value`, built only after counting that its serialized
/// JSON does not exceed `limit` bytes.
pub(super) fn structured_result<T: Serialize + ?Sized>(
    value: &T,
    limit: usize,
) -> Result<CallToolResult, ErrorData> {
    if serialized_len_within(value, limit)?.is_none() {
        return Err(result_too_large(limit));
    }
    let value = serde_json::to_value(value).map_err(serialization_error)?;
    Ok(CallToolResult::structured(value))
}

/// The serialized length of `value`, or `None` once it exceeds `limit`; nothing is buffered.
pub(super) fn serialized_len_within<T: Serialize + ?Sized>(
    value: &T,
    limit: usize,
) -> Result<Option<usize>, ErrorData> {
    let mut counter = ByteCounter { written: 0, limit };
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => Ok(Some(counter.written)),
        Err(_) if counter.written > limit => Ok(None),
        Err(error) => Err(serialization_error(error)),
    }
}

/// Whether the complete serialized tool result fits into `limit` bytes.
pub(super) fn fits_response(result: &CallToolResult, limit: usize) -> Result<bool, ErrorData> {
    Ok(serialized_len_within(result, limit)?.is_some())
}

struct ByteCounter {
    written: usize,
    limit: usize,
}

impl io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.written = self.written.saturating_add(buf.len());
        if self.written > self.limit {
            return Err(io::Error::other("serialized size limit exceeded"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialization_error(error: serde_json::Error) -> ErrorData {
    ErrorData::internal_error(
        format!("failed to serialize structured content: {error}"),
        None,
    )
}

/// The resource-limit error of a result that can be neither returned nor stored.
pub(super) fn result_too_large(limit: usize) -> ErrorData {
    ErrorData::internal_error(
        format!(
            "the tool result exceeds MAX_MCP_RESULT_CACHE_BYTES ({limit} bytes), so it can be neither returned nor stored for read_tool_result_page; narrow the request, for example with a smaller limit or a selected jar"
        ),
        None,
    )
}

/// The single error of every result identifier that cannot be read, whatever the reason.
pub(super) fn unavailable_result() -> ErrorData {
    ErrorData::invalid_params(
        "result_id is unknown, expired, evicted, or belongs to another project; repeat the original tool call",
        None,
    )
}

pub(super) fn invalid_cursor() -> ErrorData {
    ErrorData::invalid_params(
        "cursor is not a next_cursor returned for this result_id; omit it to read the first page",
        None,
    )
}

/// Returned instead of a tool result that exceeds `MAX_MCP_RESPONSE_BYTES`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct DeferredResultMarker {
    pub deferred_result: DeferredResult,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(
    description = "Present instead of the normal fields when the result exceeded MAX_MCP_RESPONSE_BYTES; read it with read_tool_result_page"
)]
pub struct DeferredResult {
    #[schemars(description = "Opaque identifier valid only in this server process")]
    pub result_id: String,
    #[schemars(description = "The tool whose result was stored")]
    pub tool: String,
    #[schemars(description = "Byte size of the stored structured JSON text")]
    pub total_bytes: usize,
    #[schemars(description = "The tool that returns the stored JSON page by page")]
    pub read_with: String,
    #[schemars(description = "Seconds until the stored result expires")]
    pub expires_in_seconds: u64,
    #[schemars(description = "How to read the stored result")]
    pub instruction: String,
}

pub(super) fn deferred_marker(
    tool: &str,
    result_id: String,
    total_bytes: usize,
) -> DeferredResultMarker {
    DeferredResultMarker {
        deferred_result: DeferredResult {
            result_id,
            tool: tool.to_owned(),
            total_bytes,
            read_with: READ_PAGE_TOOL.to_owned(),
            expires_in_seconds: STORED_RESULT_TTL.as_secs(),
            instruction: DEFERRED_INSTRUCTION.to_owned(),
        },
    }
}

/// One consecutive fragment of a stored result's JSON text.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ToolResultPage {
    #[schemars(description = "Byte offset of fragment in the stored JSON text")]
    pub offset: usize,
    #[schemars(
        description = "The next part of the stored JSON text; it never splits a UTF-8 character and is not valid JSON on its own"
    )]
    pub fragment: String,
    #[schemars(description = "Cursor of the next page; null when complete is true")]
    pub next_cursor: Option<String>,
    #[schemars(description = "True when this fragment ends the stored JSON text")]
    pub complete: bool,
    #[schemars(description = "Byte size of the complete stored JSON text")]
    pub total_bytes: usize,
}

/// Why no page could be produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PageError {
    /// Not even one character fits into the response limit.
    LimitTooSmall,
}

pub(super) fn encode_cursor(result_id: &str, offset: usize) -> String {
    format!("{result_id}.{offset}")
}

/// The offset of a cursor issued for `result_id` within `json`: a canonical encoding of a
/// character boundary before the end of the text.
pub(super) fn decode_cursor(result_id: &str, cursor: &str, json: &str) -> Option<usize> {
    let offset = cursor
        .strip_prefix(result_id)?
        .strip_prefix('.')?
        .parse::<usize>()
        .ok()?;
    let canonical = encode_cursor(result_id, offset) == cursor;
    (canonical && offset < json.len() && json.is_char_boundary(offset)).then_some(offset)
}

/// The longest page of `json` starting at `offset` whose complete serialized tool result
/// fits into `limit` bytes. `offset` must be a character boundary before the end of `json`.
pub(super) fn fit_page(
    json: &str,
    offset: usize,
    result_id: &str,
    limit: usize,
) -> Result<ToolResultPage, PageError> {
    // The skeleton carries the longest cursor this text can have and `complete: false`, so
    // its size bounds every page's non-fragment bytes.
    let skeleton = ToolResultPage {
        offset,
        fragment: String::new(),
        next_cursor: Some(encode_cursor(result_id, json.len())),
        complete: false,
        total_bytes: json.len(),
    };
    let budget = limit
        .checked_sub(page_response_len(&skeleton)?)
        .ok_or(PageError::LimitTooSmall)?;
    let mut end = offset;
    let mut cost = 0usize;
    for character in json[offset..].chars() {
        let next = cost + response_cost(character);
        if next > budget {
            break;
        }
        cost = next;
        end += character.len_utf8();
    }
    loop {
        if end == offset {
            return Err(PageError::LimitTooSmall);
        }
        let page = page(json, offset, end, result_id);
        if page_response_len(&page)? <= limit {
            return Ok(page);
        }
        // Unreachable while `response_cost` is exact; shrinking keeps the limit a guarantee.
        end = previous_boundary(json, end);
    }
}

fn page(json: &str, offset: usize, end: usize, result_id: &str) -> ToolResultPage {
    let complete = end == json.len();
    ToolResultPage {
        offset,
        fragment: json[offset..end].to_owned(),
        next_cursor: (!complete).then(|| encode_cursor(result_id, end)),
        complete,
        total_bytes: json.len(),
    }
}

fn previous_boundary(json: &str, end: usize) -> usize {
    (0..end)
        .rev()
        .find(|index| json.is_char_boundary(*index))
        .unwrap_or(0)
}

/// Bytes one fragment character adds to the tool result: escaped once inside the page's
/// structured JSON, and escaped twice inside the `content` text that repeats that JSON.
fn response_cost(character: char) -> usize {
    match character {
        '"' | '\\' => 2 + 4,
        '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2 + 3,
        control if u32::from(control) < 0x20 => 6 + 7,
        other => 2 * other.len_utf8(),
    }
}

fn page_response_len(page: &ToolResultPage) -> Result<usize, PageError> {
    let result = structured_result(page, usize::MAX).map_err(|_| PageError::LimitTooSmall)?;
    serde_json::to_vec(&result)
        .map(|bytes| bytes.len())
        .map_err(|_| PageError::LimitTooSmall)
}

/// Rejects a response limit so small that the largest possible marker, or a page with one
/// character of the largest possible stored result, would not fit.
pub(super) fn validate_response_limit(
    max_response_bytes: usize,
    max_result_bytes: usize,
    longest_tool_name: usize,
) -> anyhow::Result<()> {
    let marker = deferred_marker(
        &"t".repeat(longest_tool_name),
        longest_result_id(),
        max_result_bytes,
    );
    let marker_bytes = serialized_response_len(&marker)?;
    if marker_bytes > max_response_bytes {
        anyhow::bail!(
            "MAX_MCP_RESPONSE_BYTES ({max_response_bytes}) is too small: a deferred_result marker needs up to {marker_bytes} bytes"
        );
    }
    let fragment = WIDEST_FRAGMENT_CHARACTER.to_string();
    let page = ToolResultPage {
        offset: max_result_bytes,
        next_cursor: Some(encode_cursor(&longest_result_id(), max_result_bytes)),
        fragment,
        complete: false,
        total_bytes: max_result_bytes,
    };
    let page_bytes = serialized_response_len(&page)?;
    if page_bytes > max_response_bytes {
        anyhow::bail!(
            "MAX_MCP_RESPONSE_BYTES ({max_response_bytes}) is too small: a read_tool_result_page response with one character needs up to {page_bytes} bytes"
        );
    }
    Ok(())
}

fn serialized_response_len<T: Serialize>(value: &T) -> anyhow::Result<usize> {
    let result = CallToolResult::structured(serde_json::to_value(value)?);
    Ok(serde_json::to_vec(&result)?.len())
}

/// `schema` extended so a result may instead be a [`DeferredResultMarker`]: the marker is an
/// optional `deferred_result` property, and the original required fields become one branch
/// of an `anyOf` whose other branch requires only the marker. The root stays an object.
pub(super) fn deferrable_output_schema(schema: &JsonObject) -> JsonObject {
    let mut schema = schema.clone();
    let properties = schema
        .entry("properties")
        .or_insert_with(|| Value::Object(JsonObject::new()));
    if let Value::Object(properties) = properties {
        properties.insert(MARKER_PROPERTY.to_owned(), deferred_result_schema());
    }
    if let Some(required) = schema.remove("required") {
        schema.insert(
            "anyOf".to_owned(),
            json!([{ "required": required }, { "required": [MARKER_PROPERTY] }]),
        );
    }
    schema
}

fn deferred_result_schema() -> Value {
    let mut schema = schemars::schema_for!(DeferredResult).to_value();
    if let Value::Object(object) = &mut schema {
        object.remove("$schema");
        object.remove("title");
    }
    schema
}

#[cfg(test)]
#[path = "../../tests/unit/server_response_limit.rs"]
mod tests;
