#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    MsgIn,
    MsgOut,
    ToolCall,
    ToolResult,
    CodeRead,
    CodeEdit,
    SpanLink,
    Meta,
}
