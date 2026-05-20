#!/bin/bash
# Disable Claude Code API proxy

unset ANTHROPIC_BASE_URL

echo "CC Proxy disabled."
echo "  Anthropic API → api.anthropic.com (direct)"
