# MCP resources

`resources/list` returns one `mail://ACCOUNT_ID/` resource for each account
visible to the client; each has only `uri`, persisted account `name`, a static
description and `application/json` MIME type. The advertised URI is the only
accepted grammar: `mail://` + a non-empty ASCII account id (`A-Z`, `a-z`,
`0-9`, `.` or `-`) + exactly one trailing `/`. `resources/read` never parses,
normalizes or percent-decodes a general URL: extra path segments, query or
fragment, userinfo, port, percent escapes, a missing id or trailing slash, and
a different scheme are invalid before Core. A valid canonical URI returns the
same local account metadata projection as `get_account`, serialized in its
`contents[].text`. Both are routed through the same live Core authorization as
read tools: disabling MCP immediately returns `PERMISSION_DENIED`, even for a
running stdio process. Changing the id in a URI cannot bypass the process
account allowlist.

Malformed resource parameters are JSON-RPC `-32602`. A valid resource method
that reaches Core and fails authorization or lookup is JSON-RPC `-32000` with
`data.code` set to the corresponding Core code. This differs deliberately
from `tools/call`, whose normal domain failures are an `isError: true` result;
see [tools](tools.md#error-contract).

Message content remains behind explicit tools; resource reads never include
passwords, OAuth tokens or bodies.
