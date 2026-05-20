#!/bin/bash
# Enable Claude Code API proxy (reverse proxy mode)
# After sourcing, all Anthropic API calls will go through localhost:8888

export ANTHROPIC_BASE_URL="http://localhost:8888"

echo "CC Proxy enabled."
echo "  Anthropic API → http://localhost:8888"
echo "  Dashboard     → http://localhost:5000"
echo ""
echo "To disable: source DisableProxy.sh"
