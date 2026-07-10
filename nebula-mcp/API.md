# API

The server speaks the [Model Context Protocol](https://modelcontextprotocol.io)
over stdio using JSON-RPC 2.0, one JSON object per line. It implements protocol
revision **2024-11-05**.

## Transport

- **stdin**: newline-delimited JSON-RPC requests/notifications from the client.
- **stdout**: newline-delimited JSON-RPC responses. Nothing else is ever written
  here.
- **stderr**: human/JSON logs (and optionally rotating files).

Blank lines are ignored. Each response is written atomically with a trailing
newline.

## Methods

### `initialize`

Request:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize",
 "params":{"protocolVersion":"2024-11-05","capabilities":{},
           "clientInfo":{"name":"my-agent","version":"1.0"}}}
```

Result:

```json
{"jsonrpc":"2.0","id":1,"result":{
  "protocolVersion":"2024-11-05",
  "capabilities":{"tools":{"listChanged":false},"logging":{}},
  "serverInfo":{"name":"nebula-mcp","version":"0.1.0"},
  "instructions":"..."}}
```

### `notifications/initialized`

Client → server notification (no response) after a successful `initialize`.

### `tools/list`

Request:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
```

Result: `{ "tools": [ { "name", "description", "inputSchema", "annotations" }, … ] }`.
Only enabled tools are returned. `inputSchema` is a JSON Schema object and is the
authoritative argument reference for each tool.

### `tools/call`

Request:

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call",
 "params":{"name":"fs.read","arguments":{"path":"src/main.rs"}}}
```

Result (`CallToolResult`):

```json
{"jsonrpc":"2.0","id":3,"result":{
  "content":[{"type":"text","text":"{ ...json... }"}],
  "isError":false}}
```

Most tools return a single `text` content block containing a JSON document.
`browser.screenshot` and similar write artifacts to disk and return their paths.

### `ping`

Request `{"jsonrpc":"2.0","id":4,"method":"ping"}` → result `{}`.

### `notifications/cancelled`

Client → server notification to cancel an in-flight request:

```json
{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3}}
```

The corresponding tool call is aborted (its child process, if any, is killed)
and returns an error result with the `cancelled` category.

### `notifications/progress` (server → client)

If a `tools/call` includes a progress token in `_meta`, the server streams
progress updates for the duration of the call:

Request opting into progress:

```json
{"jsonrpc":"2.0","id":7,"method":"tools/call",
 "params":{"name":"terminal.run",
           "arguments":{"program":"cargo","args":["build"]},
           "_meta":{"progressToken":"build-1"}}}
```

Server-initiated notifications while it runs:

```json
{"jsonrpc":"2.0","method":"notifications/progress",
 "params":{"progressToken":"build-1","progress":0,"message":"started: cargo"}}
{"jsonrpc":"2.0","method":"notifications/progress",
 "params":{"progressToken":"build-1","progress":1,"message":"cargo running 3s"}}
```

Command-wrapping tools emit an immediate `started` update and periodic
heartbeats; the final result arrives as the normal `tools/call` response.

## Error model

Two distinct failure channels:

1. **Protocol errors** — returned as JSON-RPC `error` objects:

   | Code | Meaning |
   | --- | --- |
   | `-32700` | Parse error (invalid JSON frame) |
   | `-32600` | Invalid request |
   | `-32601` | Method not found / unknown or disabled tool |
   | `-32602` | Invalid params (unparseable `tools/call` params) |
   | `-32603` | Internal error |

2. **Tool execution errors** — returned as a *successful* JSON-RPC response
   whose `result` is a `CallToolResult` with `isError: true` and a text block of
   the form `"[<category>] <message>"`. This lets the model read and react to
   failures. Categories include:

   `invalid_arguments`, `permission_denied`, `path_not_allowed`,
   `command_not_allowed`, `timeout`, `cancelled`, `output_too_large`,
   `tool_not_found`, `platform_unsupported`, `io`, `execution`, `internal`.

## Example session

```
→ {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"a","version":"1"}}}
← {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05", ...}}
→ {"jsonrpc":"2.0","method":"notifications/initialized"}
→ {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"terminal.run","arguments":{"program":"cargo","args":["test"]}}}
← {"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"exitCode\":0,\"stdout\":\"...\"}"}],"isError":false}}
```
