"""MCP over HTTP for the device scripts: with MCP_URL set (the viewer's `/mcp`, e.g. `http://127.0.0.1:8202/mcp`),
run.py and tasks.py connect there instead of starting `iphone-control mcp` on stdio. A task's token goes in the
`Iphone-Control-Task-Token` header. MCP_CA: a certificate to trust for an `https://` URL (the viewer's self-signed
one, `<data dir>/viewer-tls/cert.pem`)."""

import os
import ssl
from contextlib import AsyncExitStack

import httpx2
from mcp.client.streamable_http import streamable_http_client

URL = os.environ.get("MCP_URL", "")
TOKEN_HEADER = "Iphone-Control-Task-Token"


async def streams(stack: AsyncExitStack, token: str | None):
    """(read, write) streams of an MCP session at MCP_URL, as the task with `token` (None: no task)"""
    headers = {TOKEN_HEADER: token} if token else None
    ca = os.environ.get("MCP_CA")
    verify = ssl.create_default_context(cafile=ca) if ca else True
    # the SDK's default timeouts (create_mcp_http_client), plus the certificate; never through a proxy from the
    # environment (the viewer is on this machine or the local network)
    client = httpx2.AsyncClient(
        headers=headers, timeout=httpx2.Timeout(30, read=300), verify=verify, trust_env=False
    )
    await stack.enter_async_context(client)
    s = await stack.enter_async_context(streamable_http_client(URL, http_client=client))
    return s[0], s[1]
