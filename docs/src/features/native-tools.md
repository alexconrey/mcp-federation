# Federation-Native Tools

The federation always exposes five built-in tools under the `federation`
namespace, independent of any leaf state. They are visible to every client
(including per-client RBAC entries) since they operate on federation state.

| Tool                          | Arguments                                    | Purpose                                                                                             |
|-------------------------------|----------------------------------------------|-----------------------------------------------------------------------------------------------------|
| `federation__list_servers`    | none                                         | List every configured leaf with its URL, transport, tags, health, cached tool count, and `is_federation`. |
| `federation__server_info`     | `alias` (required)                           | Details for one leaf: health, consecutive failures, cached tool names.                              |
| `federation__refresh`         | `alias` (optional)                           | Force a tool-list refresh for one leaf, or all leaves when `alias` is omitted.                      |
| `federation__search_tools`    | `query` (required), `alias` (optional)       | Case-insensitive substring match against tool names and descriptions across leaves.                 |
| `federation__topology`        | none                                         | Full federation tree -- this node plus each leaf, including an `is_federation` flag to walk nested federations. |

Under the hood these are just normal MCP tools; a client sees them in
`tools/list` alongside every leaf tool.
