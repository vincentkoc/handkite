# OpenClaw integration

OpenClaw integration is deferred. Direct MCP qualification does not prove
integration with an OpenClaw agent.

Handkite exposes its observation-only MCP entry point through
`handkite-mcp --service --capabilities observation`. Select the agent's
transport and verify its tool calls before relying on the integration.

`tools/openclaw_scope_fixture.py` is a standalone, credential-free test
utility. Its presence does not install, configure or start OpenClaw.
