#!/usr/bin/env bash
# Exercise the Responses -> Chat Completions bridge without changing user settings.
# CC_PROXY_BASE_URL=http://host:8888/v1 CC_PROXY_MODEL=deepseek-v4-flash \
#   scripts/test-codex-proxy.sh '分析代码改动内容'
set -euo pipefail

relay_base_url="${CC_PROXY_BASE_URL:-http://127.0.0.1:8888/v1}"
relay_model="${CC_PROXY_MODEL:-deepseek-v4-flash}"
if [[ $# -eq 0 ]]; then
    set -- '只回复 PROXY_CODEX_OK，不要调用工具。'
fi

# Codex Apps/subagents use Responses namespaces, and web search is a built-in tool.
# The Chat Completions bridge currently accepts function tools; use HTTP SSE.
CODEX_API_KEY="${CODEX_API_KEY:-sk-local-proxy-probe}" exec codex exec \
    --ignore-user-config --ephemeral --sandbox read-only \
    -c 'model_provider="cc_proxy"' \
    -c 'model_providers.cc_proxy.name="CC Proxy"' \
    -c "model_providers.cc_proxy.base_url=$relay_base_url" \
    -c 'model_providers.cc_proxy.env_key="CODEX_API_KEY"' \
    -c 'model_providers.cc_proxy.wire_api="responses"' \
    -c 'model_providers.cc_proxy.supports_websockets=false' \
    -c 'features.apps=false' \
    -c 'features.multi_agent=false' \
    -c 'web_search="disabled"' \
    -m "$relay_model" "$@"
