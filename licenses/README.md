# Third-party licences

TableX is licensed under AGPL-3.0. Code adapted from other projects keeps the licence of its origin;
this folder holds the texts.

## Apache-2.0 (`Apache-2.0.txt`)

Adapted from [dbx](https://github.com/t8y2/dbx) (`crates/dbx-core/src/ai.rs`, `ai_effort.rs`,
`ai_model_filter.rs`). The Apache License 2.0 allows this use; the files below carry a header that
names the origin and says they are changed.

| File | From dbx |
| --- | --- |
| `rust-engine/crates/engine-ai/src/openai/mod.rs` | `stream_openai_with_tools`, `provider_requires_api_key` |
| `rust-engine/crates/engine-ai/src/openai/endpoint.rs` | `resolve_endpoint`, `ensure_openai_version_prefix`, `resolve_model_list_endpoint`, `resolve_ollama_show_endpoint` |
| `rust-engine/crates/engine-ai/src/openai/request.rs` | `build_openai_chat_messages`, `to_openai_tool`, the request body of `stream_openai_with_tools` |
| `rust-engine/crates/engine-ai/src/openai/stream.rs` | `openai_stream_text`, `openai_stream_reasoning`, `openai_stream_has_finish_reason`, `text_from_content_value`, the stream loop of `stream_openai_with_tools`, `StreamingToolCallAccumulator` |
| `rust-engine/crates/engine-ai/src/openai/effort.rs` | `is_openai_reasoning_model`, `set_chat_completion_token_limit`, `apply_chat_completion_thinking_toggle`, `is_kimi_model`, `apply_openai_effort`, the OpenAI and Ollama effort registry, `ai_model_filter.rs` |
| `rust-engine/crates/engine-ai/src/openai/models.rs` | `parse_model_list_response`, `list_openai_compatible_models`, the Ollama `/api/show` helpers |
| `rust-engine/crates/engine-ai/src/openai/tests.rs` | the tests and payload samples of the functions above (endpoints, model lists, Ollama filter, thinking toggle, token limit, reasoning chunks, response shapes, finish without `[DONE]`) |

The unit tests inside the files above port the tests of the same dbx functions.
