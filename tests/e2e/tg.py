# /// script
# requires-python = ">=3.12"
# dependencies = ["telethon", "cryptg"]
# ///
"""
Telegram CLI client for E2E testing.

Connects as a real user account via Telethon. One-time interactive auth,
then fully scriptable from the command line.

Environment variables:
    TELEGRAM_API_ID      - from https://my.telegram.org/apps
    TELEGRAM_API_HASH    - from https://my.telegram.org/apps

Session file stored at ~/.config/pup/telethon.session

Usage:
    uv run tg.py auth                              # one-time login
    uv run tg.py me                                # show current user
    uv run tg.py send CHAT_ID TOPIC_ID "message"   # send to a topic
    uv run tg.py send CHAT_ID "message"            # send to chat (no topic)
    uv run tg.py read CHAT_ID [--topic TOPIC_ID] [--limit N]
    uv run tg.py wait CHAT_ID [--topic TOPIC_ID] [--timeout SEC] [--from USER_ID] [--contains TEXT]
    uv run tg.py topics CHAT_ID                    # list forum topics
    uv run tg.py history CHAT_ID TOPIC_ID [--limit N]  # messages in a topic
"""

import argparse
import asyncio
import json
import os
import sys
import time
import tomllib
from pathlib import Path

from telethon import TelegramClient
from telethon.tl.functions.messages import GetForumTopicsRequest, GetRepliesRequest
from telethon.tl.types import MessageService


SCRIPT_DIR = Path(__file__).resolve().parent
CONFIG_PATH = SCRIPT_DIR / "tg.toml"
SESSION_PATH = SCRIPT_DIR / ".tg-session"


def get_client() -> TelegramClient:
    api_id = None
    api_hash = None
    if CONFIG_PATH.exists():
        with open(CONFIG_PATH, "rb") as f:
            config = tomllib.load(f)
        api_id = config.get("api_id")
        api_hash = config.get("api_hash")

    # Env vars override config
    api_id = os.environ.get("TELEGRAM_API_ID", api_id)
    api_hash = os.environ.get("TELEGRAM_API_HASH", api_hash)

    if not api_id or not api_hash:
        print("error: missing api_id / api_hash", file=sys.stderr)
        print(f"create {CONFIG_PATH}:", file=sys.stderr)
        print("", file=sys.stderr)
        print("  api_id = 12345678", file=sys.stderr)
        print('  api_hash = "abc123..."', file=sys.stderr)
        print("", file=sys.stderr)
        print("get them from https://my.telegram.org/apps", file=sys.stderr)
        sys.exit(1)
    return TelegramClient(str(SESSION_PATH), int(api_id), api_hash)


async def cmd_auth(args):
    """Interactive login — run once, session is saved."""
    client = get_client()
    await client.start()
    me = await client.get_me()
    print(json.dumps({
        "id": me.id,
        "first_name": me.first_name,
        "username": me.username,
        "phone": me.phone,
    }))
    await client.disconnect()


async def cmd_me(args):
    """Show current user info."""
    client = get_client()
    await client.connect()
    if not await client.is_user_authorized():
        print("error: not logged in. run: uv run tg.py auth", file=sys.stderr)
        sys.exit(1)
    me = await client.get_me()
    print(json.dumps({
        "id": me.id,
        "first_name": me.first_name,
        "username": me.username,
        "phone": me.phone,
    }))
    await client.disconnect()


async def cmd_send(args):
    """Send a message to a chat, optionally in a specific topic."""
    client = get_client()
    await client.connect()
    if not await client.is_user_authorized():
        print("error: not logged in", file=sys.stderr)
        sys.exit(1)

    chat_id = int(args.chat_id)
    entity = await client.get_entity(chat_id)

    kwargs = {}
    if args.topic_id is not None:
        kwargs["reply_to"] = int(args.topic_id)

    msg = await client.send_message(entity, args.message, **kwargs)
    print(json.dumps({
        "message_id": msg.id,
        "chat_id": chat_id,
        "topic_id": args.topic_id,
        "text": args.message,
        "date": msg.date.isoformat() if msg.date else None,
    }))
    await client.disconnect()


async def cmd_read(args):
    """Read recent messages from a chat."""
    client = get_client()
    await client.connect()
    if not await client.is_user_authorized():
        print("error: not logged in", file=sys.stderr)
        sys.exit(1)

    chat_id = int(args.chat_id)
    entity = await client.get_entity(chat_id)
    limit = int(args.limit) if args.limit else 10

    if args.topic_id is not None:
        # Use GetReplies to get messages in a specific topic thread
        result = await client(GetRepliesRequest(
            peer=entity,
            msg_id=int(args.topic_id),
            offset_id=0,
            offset_date=None,
            add_offset=0,
            limit=limit,
            max_id=0,
            min_id=0,
            hash=0,
        ))
        messages = result.messages
    else:
        messages = await client.get_messages(entity, limit=limit)

    out = []
    for m in reversed(messages):
        entry = {
            "message_id": m.id,
            "date": m.date.isoformat() if m.date else None,
        }
        if isinstance(m, MessageService):
            entry["type"] = "service"
            entry["action"] = type(m.action).__name__ if m.action else None
        else:
            entry["type"] = "message"
            entry["text"] = m.text or ""
            if m.from_id:
                entry["from_id"] = m.from_id.user_id if hasattr(m.from_id, 'user_id') else str(m.from_id)
            if m.reply_to:
                entry["reply_to_top_id"] = getattr(m.reply_to, 'reply_to_top_id', None)
                entry["reply_to_msg_id"] = getattr(m.reply_to, 'reply_to_msg_id', None)
        out.append(entry)

    print(json.dumps(out, indent=2))
    await client.disconnect()


async def cmd_wait(args):
    """Wait for a new message matching criteria. Returns on first match or timeout."""
    client = get_client()
    await client.connect()
    if not await client.is_user_authorized():
        print("error: not logged in", file=sys.stderr)
        sys.exit(1)

    chat_id = int(args.chat_id)
    entity = await client.get_entity(chat_id)
    timeout = float(args.timeout) if args.timeout else 30.0
    from_id = int(args.from_user) if args.from_user else None
    contains = args.contains
    topic_id = int(args.topic_id) if args.topic_id else None

    deadline = time.monotonic() + timeout
    matched = None

    def check_message(m):
        """Check if a message matches the filter criteria."""
        if isinstance(m, MessageService):
            return False
        if from_id and (not m.from_id or getattr(m.from_id, 'user_id', None) != from_id):
            return False
        msg_text = m.text or m.raw_text or ""
        if contains and contains not in msg_text:
            return False
        return True

    while time.monotonic() < deadline:
        # Fetch recent messages (not just new ones) to catch responses
        # that arrived before this wait call started.
        if topic_id is not None:
            result = await client(GetRepliesRequest(
                peer=entity,
                msg_id=topic_id,
                offset_id=0,
                offset_date=None,
                add_offset=0,
                limit=20,
                max_id=0,
                min_id=0,
                hash=0,
            ))
            msgs = result.messages
        else:
            msgs = await client.get_messages(entity, limit=20)

        for m in msgs:
            if check_message(m):
                matched = m
                break

        if matched:
            break

        await asyncio.sleep(1.0)

    if matched:
        out = {
            "message_id": matched.id,
            "text": matched.text or matched.raw_text or "",
            "date": matched.date.isoformat() if matched.date else None,
        }
        if matched.from_id:
            out["from_id"] = matched.from_id.user_id if hasattr(matched.from_id, 'user_id') else str(matched.from_id)
        print(json.dumps(out))
        await client.disconnect()
        sys.exit(0)
    else:
        print(json.dumps({"error": "timeout", "timeout": timeout}))
        await client.disconnect()
        sys.exit(1)


async def cmd_topics(args):
    """List forum topics in a supergroup."""
    client = get_client()
    await client.connect()
    if not await client.is_user_authorized():
        print("error: not logged in", file=sys.stderr)
        sys.exit(1)

    chat_id = int(args.chat_id)
    entity = await client.get_entity(chat_id)

    result = await client(GetForumTopicsRequest(
        peer=entity,
        offset_date=None,
        offset_id=0,
        offset_topic=0,
        limit=100,
        q=args.query or None,
    ))

    out = []
    for topic in result.topics:
        out.append({
            "id": topic.id,
            "title": topic.title,
            "date": topic.date.isoformat() if topic.date else None,
            "icon_emoji_id": topic.icon_emoji_id if hasattr(topic, 'icon_emoji_id') else None,
        })

    print(json.dumps(out, indent=2))
    await client.disconnect()


async def cmd_history(args):
    """Read message history in a specific forum topic."""
    client = get_client()
    await client.connect()
    if not await client.is_user_authorized():
        print("error: not logged in", file=sys.stderr)
        sys.exit(1)

    chat_id = int(args.chat_id)
    topic_id = int(args.topic_id)
    entity = await client.get_entity(chat_id)
    limit = int(args.limit) if args.limit else 20

    result = await client(GetRepliesRequest(
        peer=entity,
        msg_id=topic_id,
        offset_id=0,
        offset_date=None,
        add_offset=0,
        limit=limit,
        max_id=0,
        min_id=0,
        hash=0,
    ))

    out = []
    for m in reversed(result.messages):
        entry = {
            "message_id": m.id,
            "date": m.date.isoformat() if m.date else None,
        }
        if isinstance(m, MessageService):
            entry["type"] = "service"
            entry["action"] = type(m.action).__name__ if m.action else None
        else:
            entry["type"] = "message"
            entry["text"] = m.text or ""
            entry["raw_text"] = m.raw_text or ""
            if m.from_id:
                entry["from_id"] = m.from_id.user_id if hasattr(m.from_id, 'user_id') else str(m.from_id)
            # Include any formatting entities for inspection
            if m.entities:
                entry["has_entities"] = True
            # Include reply_markup (inline keyboards) for cancel button verification
            if m.reply_markup:
                markup_type = type(m.reply_markup).__name__
                entry["reply_markup"] = {"type": markup_type}
                if hasattr(m.reply_markup, 'rows') and m.reply_markup.rows:
                    buttons = []
                    for row in m.reply_markup.rows:
                        for btn in row.buttons:
                            b = {"text": btn.text}
                            if hasattr(btn, 'data') and btn.data:
                                b["callback_data"] = btn.data.decode() if isinstance(btn.data, bytes) else btn.data
                            buttons.append(b)
                    entry["reply_markup"]["buttons"] = buttons
        out.append(entry)

    print(json.dumps(out, indent=2))
    await client.disconnect()


def main():
    parser = argparse.ArgumentParser(description="Telegram CLI for E2E testing")
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("auth", help="Interactive login (one-time)")
    sub.add_parser("me", help="Show current user info")

    p_send = sub.add_parser("send", help="Send a message")
    p_send.add_argument("chat_id")
    p_send.add_argument("topic_or_message")
    p_send.add_argument("message", nargs="?", default=None)

    p_read = sub.add_parser("read", help="Read recent messages")
    p_read.add_argument("chat_id")
    p_read.add_argument("--topic", dest="topic_id")
    p_read.add_argument("--limit", default="10")

    p_wait = sub.add_parser("wait", help="Wait for a new message")
    p_wait.add_argument("chat_id")
    p_wait.add_argument("--topic", dest="topic_id")
    p_wait.add_argument("--timeout", default="30")
    p_wait.add_argument("--from", dest="from_user")
    p_wait.add_argument("--contains")

    p_topics = sub.add_parser("topics", help="List forum topics")
    p_topics.add_argument("chat_id")
    p_topics.add_argument("--query", "-q", default=None)

    p_hist = sub.add_parser("history", help="Messages in a topic")
    p_hist.add_argument("chat_id")
    p_hist.add_argument("topic_id")
    p_hist.add_argument("--limit", default="20")

    args = parser.parse_args()

    # Normalize send args: "send CHAT TOPIC MSG" or "send CHAT MSG"
    if args.command == "send":
        if args.message is not None:
            # Three positional args: chat_id, topic_id, message
            args.topic_id = args.topic_or_message
            args.message = args.message
        else:
            # Two positional args: chat_id, message (no topic)
            args.topic_id = None
            args.message = args.topic_or_message

    dispatch = {
        "auth": cmd_auth,
        "me": cmd_me,
        "send": cmd_send,
        "read": cmd_read,
        "wait": cmd_wait,
        "topics": cmd_topics,
        "history": cmd_history,
    }

    asyncio.run(dispatch[args.command](args))


if __name__ == "__main__":
    main()
