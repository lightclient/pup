# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""
Mock pi extension — a fake IPC socket server that speaks the pup protocol.

Creates a Unix socket in the pup socket directory and responds to connections
with hello + history events, then listens for send/abort commands from the
daemon.  Can simulate agent responses (streaming message deltas).
"""

import asyncio
import json
import os
import sys
import uuid
from pathlib import Path
from dataclasses import dataclass, field
from typing import Callable, Awaitable


@dataclass
class MockExtension:
    """A fake pi session that speaks the pup IPC protocol."""

    session_id: str = field(default_factory=lambda: uuid.uuid4().hex[:12])
    session_name: str | None = None
    cwd: str = "/tmp/fake-session"
    model: str = "claude-sonnet-4-20250514"
    socket_dir: str = field(default_factory=lambda: os.path.expanduser("~/.pi/pup"))
    history_turns: list[dict] | None = None

    # Callbacks
    on_send: Callable[[str, str], Awaitable[None]] | None = None  # (message, mode)
    on_abort: Callable[[], Awaitable[None]] | None = None

    # Internal state
    _server: asyncio.Server | None = field(default=None, repr=False)
    _clients: list[asyncio.StreamWriter] = field(default_factory=list, repr=False)
    _socket_path: Path | None = field(default=None, repr=False)
    _alias_path: Path | None = field(default=None, repr=False)
    _received_messages: list[dict] = field(default_factory=list, repr=False)

    @property
    def socket_path(self) -> Path:
        return Path(self.socket_dir) / f"{self.session_id}.sock"

    @property
    def received_messages(self) -> list[dict]:
        """Messages received from the daemon."""
        return list(self._received_messages)

    async def start(self) -> None:
        """Start the mock extension socket server."""
        os.makedirs(self.socket_dir, exist_ok=True)
        self._socket_path = self.socket_path

        # Remove stale socket
        if self._socket_path.exists():
            self._socket_path.unlink()

        self._server = await asyncio.start_unix_server(
            self._handle_client, path=str(self._socket_path)
        )

        # Create alias symlink if we have a name
        if self.session_name:
            self._alias_path = Path(self.socket_dir) / f"{self.session_name}.alias"
            if self._alias_path.exists() or self._alias_path.is_symlink():
                self._alias_path.unlink()
            self._alias_path.symlink_to(f"{self.session_id}.sock")

        print(f"[mock] listening on {self._socket_path}", file=sys.stderr)

    async def stop(self) -> None:
        """Stop the server and clean up."""
        # Close all clients
        for writer in self._clients:
            try:
                writer.close()
                await writer.wait_closed()
            except Exception:
                pass
        self._clients.clear()

        if self._server:
            self._server.close()
            await self._server.wait_closed()

        # Clean up socket and alias
        if self._socket_path and self._socket_path.exists():
            self._socket_path.unlink()
        if self._alias_path and (self._alias_path.exists() or self._alias_path.is_symlink()):
            self._alias_path.unlink()

        print(f"[mock] stopped", file=sys.stderr)

    async def _handle_client(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        """Handle a new daemon connection."""
        self._clients.append(writer)
        print(f"[mock] client connected (total: {len(self._clients)})", file=sys.stderr)

        try:
            # Send hello
            await self._send_event(writer, "hello", {
                "session_id": self.session_id,
                "session_name": self.session_name,
                "cwd": self.cwd,
                "model": self.model,
            })

            # Send history
            await self._send_event(writer, "history", {
                "turns": self.history_turns or [],
                "streaming": False,
            })

            # Read commands
            while True:
                line = await reader.readline()
                if not line:
                    break
                try:
                    msg = json.loads(line.decode().strip())
                    self._received_messages.append(msg)
                    await self._handle_command(msg, writer)
                except json.JSONDecodeError:
                    continue
        except (ConnectionResetError, BrokenPipeError):
            pass
        finally:
            if writer in self._clients:
                self._clients.remove(writer)
            print(f"[mock] client disconnected (total: {len(self._clients)})", file=sys.stderr)

    async def _handle_command(self, msg: dict, writer: asyncio.StreamWriter) -> None:
        """Handle a command from the daemon."""
        msg_type = msg.get("type", "")
        msg_id = msg.get("id")

        if msg_type == "send":
            message = msg.get("message", "")
            mode = msg.get("mode", "steer")
            print(f"[mock] received send: {message!r} (mode={mode})", file=sys.stderr)

            # Send success response
            await self._send_response(writer, "send", msg_id, True)

            # Emit user_message event with echo=true (it came from pup)
            await self.broadcast_event("user_message", {
                "content": message,
                "source": "extension",
                "echo": True,
            })

            # Call the on_send callback if set
            if self.on_send:
                await self.on_send(message, mode)

        elif msg_type == "abort":
            print(f"[mock] received abort", file=sys.stderr)
            await self._send_response(writer, "abort", msg_id, True)
            if self.on_abort:
                await self.on_abort()

        elif msg_type == "get_info":
            await self._send_response(writer, "get_info", msg_id, True, data={
                "session_id": self.session_id,
                "session_name": self.session_name,
                "cwd": self.cwd,
                "model": self.model,
            })

        elif msg_type == "get_history":
            turns = msg.get("turns", 5)
            history = (self.history_turns or [])[-turns:]
            await self._send_response(writer, "get_history", msg_id, True, data={
                "turns": history,
                "streaming": False,
            })

    async def _send_event(
        self, writer: asyncio.StreamWriter, event: str, data: dict
    ) -> None:
        """Send an event to a specific client."""
        msg = {"type": "event", "event": event, "data": data}
        line = json.dumps(msg) + "\n"
        writer.write(line.encode())
        await writer.drain()

    async def _send_response(
        self,
        writer: asyncio.StreamWriter,
        command: str,
        msg_id: str | None,
        success: bool,
        data: dict | None = None,
        error: str | None = None,
    ) -> None:
        """Send a command response."""
        msg = {"type": "response", "command": command, "success": success}
        if msg_id is not None:
            msg["id"] = msg_id
        if data is not None:
            msg["data"] = data
        if error is not None:
            msg["error"] = error
        line = json.dumps(msg) + "\n"
        writer.write(line.encode())
        await writer.drain()

    async def broadcast_event(self, event: str, data: dict) -> None:
        """Broadcast an event to all connected clients."""
        msg = {"type": "event", "event": event, "data": data}
        line = json.dumps(msg) + "\n"
        encoded = line.encode()
        dead = []
        for writer in self._clients:
            try:
                writer.write(encoded)
                await writer.drain()
            except (ConnectionResetError, BrokenPipeError):
                dead.append(writer)
        for w in dead:
            self._clients.remove(w)

    async def simulate_agent_response(
        self,
        text: str,
        chunk_size: int = 20,
        chunk_delay: float = 0.05,
        message_id: str | None = None,
    ) -> None:
        """Simulate a streaming agent response."""
        msg_id = message_id or uuid.uuid4().hex[:8]

        # agent_start
        await self.broadcast_event("agent_start", {})
        # turn_start
        await self.broadcast_event("turn_start", {"turn_index": 0})
        # message_start
        await self.broadcast_event("message_start", {
            "role": "assistant",
            "message_id": msg_id,
        })

        # Stream deltas
        for i in range(0, len(text), chunk_size):
            chunk = text[i : i + chunk_size]
            await self.broadcast_event("message_delta", {
                "message_id": msg_id,
                "text": chunk,
            })
            await asyncio.sleep(chunk_delay)

        # message_end
        await self.broadcast_event("message_end", {
            "message_id": msg_id,
            "role": "assistant",
            "content": text,
        })
        # turn_end
        await self.broadcast_event("turn_end", {"turn_index": 0})
        # agent_end
        await self.broadcast_event("agent_end", {})

    async def simulate_tool_call(
        self,
        tool_name: str,
        args: dict,
        result: str,
        is_error: bool = False,
        tool_call_id: str | None = None,
    ) -> None:
        """Simulate a tool execution."""
        tc_id = tool_call_id or uuid.uuid4().hex[:8]

        await self.broadcast_event("tool_start", {
            "tool_call_id": tc_id,
            "tool_name": tool_name,
            "args": args,
        })

        await asyncio.sleep(0.1)

        await self.broadcast_event("tool_end", {
            "tool_call_id": tc_id,
            "tool_name": tool_name,
            "content": result,
            "is_error": is_error,
        })

    async def send_notification(self, text: str) -> None:
        """Broadcast a notification event (e.g., for unsupported command errors)."""
        await self.broadcast_event("notification", {"text": text})

    async def change_name(self, new_name: str) -> None:
        """Simulate a session name change."""
        old_name = self.session_name
        self.session_name = new_name

        # Update alias symlink
        if self._alias_path and (self._alias_path.exists() or self._alias_path.is_symlink()):
            self._alias_path.unlink()
        if new_name:
            self._alias_path = Path(self.socket_dir) / f"{new_name}.alias"
            if self._alias_path.exists() or self._alias_path.is_symlink():
                self._alias_path.unlink()
            self._alias_path.symlink_to(f"{self.session_id}.sock")

        await self.broadcast_event("session_name_changed", {"name": new_name})


async def _demo():
    """Demo: start a mock extension that auto-responds to messages."""
    ext = MockExtension(session_name="e2e-test")

    async def on_send(message: str, mode: str):
        await asyncio.sleep(0.5)
        await ext.simulate_agent_response(
            f"I received your message: {message!r}. This is a simulated response "
            f"from the mock extension to test the full E2E pipeline."
        )

    ext.on_send = on_send
    await ext.start()

    print(f"[mock] session_id={ext.session_id}", file=sys.stderr)
    print(f"[mock] socket={ext.socket_path}", file=sys.stderr)
    print(f"[mock] Press Ctrl+C to stop", file=sys.stderr)

    try:
        await asyncio.Event().wait()
    except asyncio.CancelledError:
        pass
    finally:
        await ext.stop()


if __name__ == "__main__":
    try:
        asyncio.run(_demo())
    except KeyboardInterrupt:
        pass
