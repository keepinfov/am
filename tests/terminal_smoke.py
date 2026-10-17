"""Unix PTY smoke test. Run after `cargo build --locked`.

Standard library only; cursor-query replies are fixed at 1,1. Assertions cover
persisted semantics and repainted text, not emulator geometry or visual blinking.
The slow offline provider fixture supplies a real, journaled inline tool boundary;
there is no client-side batching timer and no network or actual tool execution.
"""

import errno
import fcntl
import json
import os
import pty
import re
import select
import signal
import sqlite3
import struct
import subprocess
import tempfile
import termios
import time
from pathlib import Path


def main():
    root = Path(__file__).resolve().parents[1]
    temp_parent = "/tmp/opencode" if Path("/tmp/opencode").is_dir() else None
    with tempfile.TemporaryDirectory(prefix="am-pty-", dir=temp_parent) as directory:
        database = Path(directory) / "session.sqlite3"
        preferences = Path(directory) / "preferences.toml"
        slow_config = Path(directory) / "slow.toml"
        slow_config.write_text("[provider]\ndelay_ms = 300\nfirst_event_ms = 800\n")
        master, slave = pty.openpty()
        original = termios.tcgetattr(slave)
        output, pending = bytearray(), bytearray()
        child = None

        def pump(seconds=0.1):
            until = time.monotonic() + seconds
            while time.monotonic() < until:
                readable, _, _ = select.select([master], [], [], 0.02)
                if not readable:
                    continue
                try:
                    block = os.read(master, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                output.extend(block)
                pending.extend(block)
                # Retain split cursor queries across PTY read boundaries.
                while b"\x1b[6n" in pending:
                    where = pending.index(b"\x1b[6n")
                    del pending[: where + 4]
                    os.write(master, b"\x1b[1;1R")
                if len(pending) > 4096:
                    del pending[:-32]

        def keys(value, seconds=0.15):
            start = len(output)
            os.write(master, value)
            pump(seconds)
            return start

        def plain(start=0):
            return re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", bytes(output[start:]))

        def compact(start):
            # Ratatui emits cursor movement instead of some padding spaces.
            return re.sub(rb"\s", b"", plain(start))

        def repaint(width=100, height=28, *, last_frame=False):
            start = len(output)
            for columns in (width - 1, width):
                latest = len(output)
                fcntl.ioctl(
                    slave,
                    termios.TIOCSWINSZ,
                    struct.pack("HHHH", height, columns, 0, 0),
                )
                child.send_signal(signal.SIGWINCH)
                pump(0.1)
            return latest if last_frame else start

        def assert_text(start, text):
            assert re.sub(rb"\s", b"", text.encode()) in compact(start), (
                text,
                plain(start),
            )

        def assert_inline(start):
            block = bytes(output[start:])
            assert b"\x1b[?1049h" not in block, (
                "ordinary panel entered alternate screen"
            )
            assert not re.search(rb"\x1b\[\?(?:1000|1002|1003|1006|1015)h", block), (
                "inline UI captured mouse"
            )

        def rows(query, params=()):
            try:
                with sqlite3.connect(database, timeout=0.1) as connection:
                    return connection.execute(query, params).fetchall()
            except sqlite3.OperationalError:
                return []

        def wait_for(predicate, seconds=10, full_repaint=False):
            until = time.monotonic() + seconds
            while time.monotonic() < until:
                pump(0.04)
                if full_repaint:
                    repaint()
                if predicate():
                    return
                assert child.poll() is None, plain()[-4096:].decode(errors="replace")
            raise AssertionError(
                "PTY deadline exceeded: " + plain()[-4096:].decode(errors="replace")
            )

        def receipts(name):
            return [
                (rowid, command[name], reply)
                for rowid, raw_command, raw_reply in rows(
                    "SELECT rowid,command,reply FROM command_results ORDER BY rowid"
                )
                for command, reply in [(json.loads(raw_command), json.loads(raw_reply))]
                if isinstance(command, dict) and name in command
            ]

        def notice_count(text, status="info"):
            return len(
                rows(
                    "SELECT seq FROM history WHERE kind='notice' AND attempt_id IS NULL "
                    "AND text=? AND status=?",
                    (text, status),
                )
            )

        def wait_notice(text, count=1, category="settings", status="info"):
            wait_for(lambda: notice_count(text, status) == count)
            body = {"text": text, "level": status.title()}
            if category is not None:
                body["category"] = category
            recorded = [
                (rowid, reply)
                for rowid, command, reply in receipts(
                    "RecordAction" if category is not None else "RecordNotice"
                )
                if command == body
            ]
            assert len(recorded) == count, (text, recorded)
            assert all(
                reply.get("Notice", {}).get("text") == text
                and reply["Notice"].get("status") == status
                and reply["Notice"].get("attempt_id") == ""
                for _, reply in recorded
            ), recorded

        def composer(text, cursor):
            wait_for(
                lambda: rows("SELECT text,cursor FROM composer") == [(text, cursor)]
            )

        def input_ready():
            def ready():
                frame = compact(repaint(last_frame=True))
                return b"/help" in frame and b"savinginput" not in frame

            wait_for(ready)

        def submit_plain(text):
            before = len(receipts("SubmitInputAccumulated"))
            keys(text.encode() + b"\r", 0.04)
            wait_for(lambda: len(receipts("SubmitInputAccumulated")) == before + 1)
            _, body, reply = receipts("SubmitInputAccumulated")[-1]
            assert body["source"] == body["text"] == text, body
            assert body["mode"] == "after" and reply == "Ack", (body, reply)
            # Commit precedes client ACK processing. A new complete Root frame,
            # not an already-empty SQLite composer, proves the input is unlocked.
            input_ready()

        def start(config=None):
            nonlocal child
            assert child is None or child.poll() is not None
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 28, 100, 0, 0))
            pending.clear()
            args = [
                str(root / "target/debug/am"),
                "--session",
                str(database),
                "--preferences",
                str(preferences),
            ]
            if config is not None:
                args += ["--config", str(config)]
            child = subprocess.Popen(
                args,
                stdin=slave,
                stdout=slave,
                stderr=slave,
                close_fds=True,
                env={**os.environ, "TERM": "xterm-256color"},
            )
            wait_for(lambda: bool(rows("SELECT text FROM composer")))
            pump(0.4)

        def exited():
            wait_for(lambda: child.poll() is not None)
            assert child.wait(timeout=2) == 0, plain()[-4096:].decode(errors="replace")
            assert termios.tcgetattr(slave) == original, "terminal modes not restored"
            assert b"\x1b[?2004l" in output, "bracketed paste not disabled"

        def command(text):
            # The secondary field leaves the primary composer/caret untouched.
            return keys(b"\x10" + text.encode() + b"\r")

        def stop(*, paused, chord=None):
            active = rows("SELECT id FROM attempts WHERE status='running'")
            assert len(active) <= 1, active
            attempt = active[0][0] if active else None
            expected_paused = bool(rows("SELECT paused FROM session")[0][0])
            before = len(receipts("StopPending"))
            if chord is None:
                command("/stop")
            else:
                keys(chord, 0.1)
            wait_for(lambda: len(receipts("StopPending")) == before + 1)
            receipt = receipts("StopPending")[-1]
            assert receipt[1] == {
                "expected_attempt_id": attempt,
                "expected_paused": expected_paused,
            }, receipt
            assert receipt[2] == {
                "Stopped": {"paused": paused, "cancelled": attempt is not None}
            }, receipt
            wait_for(
                lambda: (
                    rows("SELECT paused FROM session") == [(int(paused),)]
                    and (
                        attempt is None
                        or rows("SELECT status FROM attempts WHERE id=?", (attempt,))
                        == [("cancelled",)]
                    )
                )
            )
            if paused:
                assert not rows("SELECT id FROM attempts WHERE status='running'")
            return attempt

        def assert_held(attempt, message_id, text):
            assert rows("SELECT status FROM attempts WHERE id=?", (attempt,)) == [
                ("running",)
            ]
            assert rows(
                "SELECT text,state FROM messages WHERE id=?", (message_id,)
            ) == [(text, "queued")]
            assert not rows(
                "SELECT id FROM attempts WHERE message_id=?", (message_id,)
            ), "pending input delivered before completion"
            assert not rows("SELECT id FROM attempts WHERE status='steered'"), (
                "pending input steered generation"
            )
            assert not rows(
                "SELECT seq FROM events WHERE kind='attempt_finished' AND attempt_id=?",
                (attempt,),
            )

        def running(text):
            wait_for(
                lambda: bool(
                    rows(
                        "SELECT a.id FROM attempts a JOIN messages m "
                        "ON m.id=a.message_id WHERE m.text=? AND a.status='running'",
                        (text,),
                    )
                )
            )
            return rows(
                "SELECT a.id FROM attempts a JOIN messages m ON m.id=a.message_id "
                "WHERE m.text=? AND a.status='running'",
                (text,),
            )[0][0]

        def tool_wait(attempt):
            wait_for(
                lambda: bool(
                    rows(
                        "SELECT seq FROM events WHERE kind='fixture_tool_started' "
                        "AND attempt_id=?",
                        (attempt,),
                    )
                ),
                seconds=12,
            )
            assert not rows(
                "SELECT seq FROM events WHERE kind='fixture_tool_finished' "
                "AND attempt_id=?",
                (attempt,),
            ), "missed fixture tool wait"

        def acquire(message_id):
            text, revision = rows(
                "SELECT text,revision FROM messages WHERE id=?", (message_id,)
            )[0]
            keys(b"\x1b[A", 0.08)
            wait_for(
                lambda: (
                    rows(
                        "SELECT state,revision FROM messages WHERE id=?", (message_id,)
                    )
                    == [("editing", revision + 1)]
                )
            )
            wait_for(
                lambda: any(
                    body == {"id": message_id, "revision": revision}
                    and reply
                    == {
                        "Editing": {
                            "id": message_id,
                            "revision": revision + 1,
                            "text": text,
                            "state": "Editing",
                        }
                    }
                    for _, body, reply in receipts("BeginEdit")
                )
            )
            pump(0.08)

        def assert_once():
            assert not rows(
                "SELECT message_id,COUNT(*) FROM attempts GROUP BY message_id HAVING COUNT(*)>1"
            ), "input delivered twice"
            assert not rows(
                "SELECT a.message_id,COUNT(*) FROM history h JOIN attempts a ON a.id=h.attempt_id "
                "WHERE h.kind='user' GROUP BY a.message_id HAVING COUNT(*)>1"
            ), "user input published twice"

        try:
            start()
            keys(b"\x1bOP")  # F1 is not a default binding.
            idle = repaint()
            assert_text(idle, "ready")
            assert not re.search(rb"ready \d+\.\ds", plain(idle)), (
                "idle status has a timer"
            )
            assert not any(
                chr(ch).encode() in plain(idle) for ch in range(0x2800, 0x2900)
            ), "idle spinner"
            assert not any(
                chr(ch).encode() in plain(idle) for ch in range(0x2581, 0x2589)
            ), "idle bars"
            assert_inline(0)

            # Real modified-arrow sequences move by words, including UTF-8 byte offsets.
            words = "one \u03bb two"
            keys(words.encode())
            composer(words, len(words.encode()))
            keys(b"\x1b[1;2D")  # Shift+Left
            composer(words, len("one \u03bb ".encode()))
            keys(b"\x1b[1;2D")
            composer(words, 4)
            keys(b"\x1b[1;2C")  # Shift+Right
            composer(words, len("one \u03bb".encode()))
            keys(b"\x1b[1;2C")
            composer(words, len(words.encode()))
            keys(b"\x15")

            # Bracketed paste preserves Unicode/newlines while dropping controls.
            text = "first \u03bb\nsecond \U0001f600"
            keys(b"\x1b[200~" + text.encode() + b"\x00\x07\x1b[201~\r")
            wait_for(lambda: rows("SELECT text FROM messages") == [(text,)])
            wait_for(lambda: rows("SELECT status FROM attempts") == [("completed",)])
            assert_once()

            # Unified history previews the durable offline fixture artifact.
            opened = keys(b"\x0f")
            assert b"\x1b[?1049h" in output[opened:]
            wait_for(lambda: b"action#" in compact(opened), full_repaint=True)
            action_rows = rows(
                "SELECT output_id FROM actions ORDER BY seq DESC LIMIT 50"
            )[::-1]
            artifact_index = next(
                index for index, row in enumerate(action_rows) if row[0]
            )
            keys(b"\x1b[B" * (artifact_index + 1) + b"\r")
            preview = len(output)
            wait_for(
                lambda: b"NoAPIortoolsareconnectedyet." in compact(preview),
                full_repaint=True,
            )
            assert_text(repaint(), "Preview")
            artifact_id = action_rows[artifact_index][0]
            artifact_bytes = rows(
                "SELECT bytes FROM artifacts WHERE id=?", (artifact_id,)
            )[0][0]
            artifact_text = Path(
                str(database) + ".artifacts", artifact_id + ".txt"
            ).read_bytes()
            assert len(artifact_text) == artifact_bytes
            assert b"No API or tools are connected yet." in artifact_text
            keys(b" ")
            assert_text(repaint(), "collapsed")
            keys(b" \x1b[6~\x1b[5~\t")
            keys(b"\x1b", 0.12)
            pump(0.5)
            settled = len(output)
            pump(0.4)
            assert b"No API or tools are connected yet." not in plain(settled), (
                "history replayed settled output"
            )

            protected = "panel protected draft"
            keys(protected.encode() + b"\x01\x1b[C")
            composer(protected, 1)
            panel_start = command("/help")
            assert_text(repaint(), "[All]")
            help_screen = repaint()
            assert_text(help_screen, "/stop")
            for removed in (
                "/pause",
                "/resume",
                "/queue",
                "/force",
                "/mode",
                "/run",
                "/interrupt",
            ):
                assert removed.encode() not in plain(help_screen), (
                    removed,
                    plain(help_screen),
                )
            keys(b"settings")
            assert_text(repaint(), "/settings")
            keys(b"\r")
            assert_text(repaint(), "Settings / 4 UI preferences")
            keys(b"\x1b", 0.12)
            assert_text(repaint(), "/settings")
            keys(b"\x1b", 0.12)
            keys(b"\x1b", 0.12)  # Close the restored secondary field, not Stop.
            keys(b"X")
            composer("pXanel protected draft", 2)
            assert len(rows("SELECT id FROM messages")) == 1
            assert_inline(panel_start)
            keys(b"\x15")

            # /s is ambiguous; first Tab opens an unselected picker, then Down
            # previews /send. Accepting a completion never executes the command.
            keys(b"/s\t")
            picker = repaint()
            for name in ("/send", "/settings", "/stop"):
                assert_text(picker, name)
            keys(b"\x1b[B\r")
            composer("/send ", 6)
            assert len(rows("SELECT id FROM messages")) == 1
            assert_inline(picker)
            keys(b"\x15")

            keys(b"/settings\t")
            assert b"Settings/4UIpreferences" not in compact(repaint())
            keys(b"\r")
            settings_start = repaint()
            for label in (
                "Settings / 4 UI preferences",
                "Status line",
                "Save as defaults",
                "Bindings",
            ):
                assert_text(settings_start, label)
            for label, field in (
                ("Status line", "status-line"),
                ("Prompt history", "prompt-history"),
            ):
                keys(b" ")
                wait_notice(f"{field} disabled")
                feedback = repaint()
                assert_text(feedback, f"{label}    [off]")
                assert_text(feedback, f"{label}: on -> off")
                assert_text(feedback, f"{field} disabled")
                keys(b" ")
                wait_notice(f"{field} enabled")
                feedback = repaint()
                assert_text(feedback, f"{label}    [on]")
                assert_text(feedback, f"{label}: off -> on")
                keys(b"\x1b[B")
            pump(3.1)
            assert_text(repaint(), "Prompt history: off -> on")
            before_enum = rows("SELECT * FROM history ORDER BY seq")
            keys(b"\r\x1b[B")  # Preview another enum value; Esc must not apply it.
            enum = repaint()
            assert_text(enum, "\u25cf bars")
            assert_text(enum, "\u25cb braille")
            assert_text(enum, "\u25cb minimal")
            keys(b"\x1b", 0.12)
            assert_text(repaint(), "Status preset    bars")
            assert rows("SELECT * FROM history ORDER BY seq") == before_enum
            keys(b"\x1b", 0.12)
            assert_inline(settings_start)
            assert not preferences.exists(), "interactive settings saved defaults"

            # Literal key/value and option completion only execute on dispatch Enter.
            keys(b"/settings \t")
            catalog = repaint()
            for field in ("status-line", "prompt-history", "status", "status-color"):
                assert_text(catalog, field)
            assert b"Apply" not in plain(catalog)
            keys(b"\x1b", 0.12)
            keys(b"\x15/settings line\tf\t\r")
            wait_notice("status-line disabled", count=2)
            keys(b"/settings prompt\tf\t\r")
            wait_notice("prompt-history disabled", count=2)
            keys(b"/settings sta\t")
            assert_text(repaint(), "status-color")
            keys(b"\r br\t\r")
            wait_notice("status set to braille")
            keys(b"/settings status braille --d\x13")
            assert_text(repaint(), "--default    Make default")
            keys(b"\x1b", 0.12)
            keys(b"\t")
            assert not preferences.exists(), "completion saved preferences"
            keys(b"\r")
            wait_notice("status set to braille (default)")
            wait_for(preferences.exists)
            saved = preferences.read_text()
            assert re.search(r'^status_preset\s*=\s*"braille"$', saved, re.M), saved
            for field in ("status_line", "prompt_history", "status_color"):
                assert field not in saved, "session-only setting persisted: " + field
            keys(b"/settings status minimal\r")
            wait_notice("status set to minimal")
            assert preferences.read_text() == saved
            keys(b"/settings status-line true\r/settings prompt-history true\r")
            wait_notice("status-line enabled", count=2)
            wait_notice("prompt-history enabled", count=2)

            # Removed commands/dispatch flags are errors, never hidden model input.
            model_messages = rows("SELECT * FROM messages ORDER BY seq")
            for invalid in (
                "/unknown",
                "/pause",
                "/queue",
                "/mode after",
                "/force",
                "/run",
                "/send 01234567 --run",
                "/send 01234567 --hold",
                "/send 01234567 --after",
                "/send 01234567 --force",
                "/send 01234567 --steer",
            ):
                previous = len(
                    rows(
                        "SELECT seq FROM history WHERE kind='notice' AND status='error'"
                    )
                )
                keys(invalid.encode() + b"\r")
                wait_for(
                    lambda: (
                        len(
                            rows(
                                "SELECT seq FROM history WHERE kind='notice' AND status='error'"
                            )
                        )
                        == previous + 1
                    )
                )
                keys(b"\x15")
            assert rows("SELECT * FROM messages ORDER BY seq") == model_messages

            # Pinned command and primary fields retain independent text and carets.
            keys(b"pinned primary\x01\x10/commands keep\r")
            keys(b"/settings sta\x1biX")
            composer("Xpinned primary", 1)
            keys(b"\x1bi")
            pinned = repaint()
            assert_text(pinned, "(pinned)")
            assert_text(pinned, "/settings sta")
            keys(b"\x01\x0b/exit\rY")
            composer("XYpinned primary", 2)
            assert child.poll() is None

            # Remaining draft commands save raw text and consume exactly once.
            command("/draft save")
            wait_notice("draft saved", category="drafts")
            wait_for(lambda: len(rows("SELECT id FROM drafts")) == 1)
            draft_id = rows("SELECT id FROM drafts")[0][0]
            assert re.fullmatch(r"[0-9a-f-]{36}", draft_id)
            composer("", 0)
            keys(b"/drafts\r")
            assert_text(repaint(), "XYpinned primary")
            keys(b"\r")
            composer("XYpinned primary", len("XYpinned primary"))
            keys(b"\x15")
            keys(f"/send {draft_id[:8]}".encode() + b"\t")
            assert rows("SELECT id FROM drafts") == [(draft_id,)]
            keys(b"\r")
            wait_for(lambda: not rows("SELECT id FROM drafts"))
            wait_for(
                lambda: (
                    rows("SELECT state FROM messages WHERE text='XYpinned primary'")
                    == [("handled",)]
                )
            )
            assert_once()
            for raw, submitted in (
                ("//literal fixture", "/literal fixture"),
                (
                    "/quit\nliteral multiline fixture",
                    "/quit\nliteral multiline fixture",
                ),
            ):
                keys(b"\x1b[200~" + raw.encode() + b"\x1b[201~\r")
                wait_for(
                    lambda: (
                        rows("SELECT state FROM messages WHERE text=?", (submitted,))
                        == [("handled",)]
                    )
                )
                assert child.poll() is None, "literal slash text executed a command"
            assert_once()

            # Async history search accepts an anchor first, opens only on next Enter.
            history_messages = rows("SELECT * FROM messages ORDER BY seq")
            history_attempts = rows("SELECT * FROM attempts ORDER BY id")
            anchor = rows(
                "SELECT seq FROM history WHERE kind='user' AND text='XYpinned primary'"
            )[0][0]
            keys(b"/history XYpinned primary\t")
            composer(f"/history {anchor}", len(f"/history {anchor}"))
            assert_text(repaint(), f"{anchor} user    XYpinned primary")
            accepted = keys(b"\r")
            assert_inline(accepted)
            opened = keys(b"\r")
            wait_for(
                lambda: f"Message#{anchor}".encode() in compact(opened),
                full_repaint=True,
            )
            assert b"\x1b[?1049h" in output[opened:]
            assert b"\x1b[?1000h" in output[opened:]
            assert_text(repaint(), "History / messages + actions")
            narrow = repaint(width=60, height=18)
            assert_text(narrow, "Conversations / Actions")
            keys(b"\r")
            assert_text(repaint(width=60, height=18), f"Message #{anchor}")
            back = keys(b"\x1b", 0.12)
            assert b"\x1b[?1049l" not in output[back:], "preview Esc closed the reader"
            assert_text(repaint(width=60, height=18), "Conversations / Actions")
            closed = keys(b"\x1b", 0.12)
            assert (
                b"\x1b[?1049l" in output[closed:] and b"\x1b[?1000l" in output[closed:]
            )
            repaint()
            assert rows("SELECT * FROM messages ORDER BY seq") == history_messages
            assert rows("SELECT * FROM attempts ORDER BY id") == history_attempts
            keys(b"LITERAL FIX\x1b[A")
            composer("//literal fixture", len("//literal fixture"))
            keys(b"\x1b[B")
            composer("LITERAL FIX", 11)
            keys(b"\x15")

            # Visual chord capture and nested completion save only on separate Enter.
            keys(b"/bindings\rn\x1bz/settings status mini\t\r")
            assert not notice_count("Binding updated for this client")
            keys(b"; /settings status-color tr\t\r")
            assert not notice_count("Binding updated for this client")
            keys(b"\r")
            wait_notice("Binding updated for this client", category="bindings")
            keys(b"\x1b", 0.12)
            keys(
                b"/bind alt+z /settings status minimal ; /settings status-color true --default\r"
            )
            wait_notice("Binding defaults saved", category="bindings")
            assert 'key = "alt+z"' in preferences.read_text()
            keys(b"/settings status braille --default\r")
            wait_notice("status set to braille (default)", count=2)
            assert 'key = "alt+z"' in preferences.read_text(), (
                "UI save replaced bindings"
            )
            keys(b"macro composer\x01\x1bz")
            wait_notice("status set to minimal", count=2)
            wait_notice("status-color enabled")
            composer("macro composer", 0)
            saved = preferences.read_text()

            # Save an exact Unicode draft/caret, then exercise confirmed exit/save ACK.
            recovered = "recover \u03bb composer"
            keys(b"\x15" + recovered.encode() + b"\x01\x1b[C\x1b[C")
            composer(recovered, 2)
            keys(b"\x03")
            assert child.poll() is None
            keys(b"X\x7f")  # Ordinary input disarms the first Ctrl+C.
            composer(recovered, 2)
            command("/settings status braille --default")
            keys(b"\x03\x03", 0)
            exited()
            wait_notice("status set to braille (default)", count=3)
            assert preferences.read_text() == saved
            durable_notices = rows(
                "SELECT seq,text,status FROM history WHERE kind='notice' ORDER BY seq"
            )

            start(root / "examples/keybindings.toml")
            composer(recovered, 2)
            assert rows("SELECT paused FROM session") == [(1,)]
            assert rows("SELECT * FROM messages ORDER BY seq") == history_messages
            assert all(
                entry in rows("SELECT seq,text,status FROM history WHERE kind='notice'")
                for entry in durable_notices
            ), "command outcomes disappeared after reopen"
            keys(b"X")
            composer("reXcover \u03bb composer", 3)
            for field, value in (
                ("status", "braille"),
                ("status-line", "true"),
                ("prompt-history", "true"),
                ("status-color", "true"),
            ):
                keys(b"\x1bp" + f"/settings {field} ".encode() + b"\t")
                assert_text(repaint(), f"{value}    Current value")
                keys(b"\x1b", 0.12)
                keys(b"\x1b", 0.12)
            disabled = keys(b"\x0f")
            assert_inline(disabled)
            for chord in (b"\x1bh", b"\x1bOQ"):
                opened = keys(chord)
                assert b"\x1b[?1049h" in output[opened:], (
                    "configured history did not open"
                )
                assert_text(repaint(), "History / messages + actions")
                keys(b"\x1b", 0.12)
            keys(b"\x1bpdetails\r")
            assert_text(repaint(), "Details")
            keys(b"\x1b", 0.12)
            keys(b"\x1bp/quit\r", 0)
            exited()

            start()  # Saved macro survives without the explicit keybindings config.
            keys(b"\x1bz")
            wait_notice("status set to minimal", count=3)
            wait_notice("status-color enabled", count=2)
            composer("reXcover \u03bb composer", 3)
            assert rows("SELECT * FROM messages ORDER BY seq") == history_messages
            assert preferences.read_text() == saved
            command("/quit")
            exited()

            # A short default turn completes naturally, including its inline action,
            # before the accumulated payload is automatically delivered.
            database = Path(directory) / "natural.sqlite3"
            preferences = Path(directory) / "natural-preferences.toml"
            start()
            input_ready()
            submit_plain("natural source")
            natural_source = running("natural source")
            submit_plain("natural first")
            composer("", 0)
            submit_plain("natural second")
            natural_text = "natural first\n\nnatural second"
            wait_for(
                lambda: (
                    rows("SELECT text FROM messages WHERE state='queued'")
                    == [(natural_text,)]
                )
            )
            natural_id = rows("SELECT id FROM messages WHERE state='queued'")[0][0]
            assert_held(natural_source, natural_id, natural_text)
            wait_for(
                lambda: bool(
                    rows(
                        "SELECT seq FROM events WHERE kind='fixture_tool_finished' AND attempt_id=?",
                        (natural_source,),
                    )
                )
            )
            assert_held(natural_source, natural_id, natural_text)
            wait_for(
                lambda: (
                    rows("SELECT status FROM attempts WHERE id=?", (natural_source,))
                    == [("completed",)]
                    and bool(
                        rows(
                            "SELECT id FROM attempts WHERE message_id=?", (natural_id,)
                        )
                    )
                )
            )
            tool_finished = rows(
                "SELECT seq FROM events WHERE kind='fixture_tool_finished' AND attempt_id=?",
                (natural_source,),
            )[0][0]
            turn_finished = rows(
                "SELECT seq FROM events WHERE kind='attempt_finished' AND attempt_id=? AND payload='completed'",
                (natural_source,),
            )[0][0]
            delivery = rows(
                "SELECT seq FROM events WHERE kind='attempt_started' AND payload=?",
                (natural_id,),
            )[0][0]
            assert tool_finished < turn_finished < delivery, (
                "auto-delivery preceded full generation/action completion"
            )
            assert rows(
                "SELECT status FROM actions WHERE attempt_id=?", (natural_source,)
            ) == [("completed",)]
            assert rows(
                "SELECT h.text FROM history h JOIN attempts a ON a.id=h.attempt_id "
                "WHERE h.kind='user' AND a.message_id=?",
                (natural_id,),
            ) == [(natural_text,)]
            wait_for(
                lambda: (
                    rows("SELECT status FROM attempts ORDER BY rowid")
                    == [("completed",), ("completed",)]
                )
            )
            assert rows("SELECT paused FROM session") == [(0,)]
            assert all(
                body["mode"] == "after"
                for _, body, _ in receipts("SubmitInputAccumulated")
            )
            assert_once()
            # Completion is durable before a transient acceptance notice expires.
            wait_for(lambda: b"ready" in compact(repaint()))
            idle = repaint()
            assert_text(idle, "ready")
            assert not any(
                chr(ch).encode() in plain(idle) for ch in range(0x2581, 0x2589)
            ), "completed turn kept bars animating"
            command("/settings status bars --default")
            wait_notice("status set to bars (default)")
            wait_for(preferences.exists)
            bars_saved = preferences.read_text()
            assert re.search(r'^status_preset\s*=\s*"bars"$', bars_saved, re.M), (
                bars_saved
            )
            command("/quit")
            exited()
            start()
            current_value = keys(b"\x10/settings status \t")
            wait_for(
                lambda: b"barsCurrentvalue" in compact(current_value), full_repaint=True
            )
            keys(b"\x1b", 0.12)
            keys(b"\x1b", 0.12)
            assert preferences.read_text() == bars_saved
            command("/quit")
            exited()

            # Slower chunks expose the tool boundary and continuing generation;
            # queued input must not steer, even after that boundary has passed.
            database = Path(directory) / "pending.sqlite3"
            preferences = Path(directory) / "pending-preferences.toml"
            start(slow_config)
            input_ready()
            submit_plain("batch source")
            source = running("batch source")
            submit_plain("batch first")
            composer("", 0)
            submit_plain("batch second")
            combined = "batch first\n\nbatch second"
            wait_for(
                lambda: (
                    rows("SELECT text,state FROM messages WHERE state='queued'")
                    == [(combined, "queued")]
                )
            )
            batch_id = rows("SELECT id FROM messages WHERE state='queued'")[0][0]
            assert rows("SELECT COUNT(*) FROM messages") == [(2,)]
            assert_text(repaint(), "\u250b batch first")
            assert_held(source, batch_id, combined)
            tool_wait(source)
            frames = []
            for _ in range(3):
                frame = repaint()
                screen = plain(frame).decode(errors="replace")
                bars = re.findall(r"[\u2581-\u2588]{3}", screen)
                assert bars, screen
                assert not re.search(r"[\u2581-\u2588]{4}", screen), screen
                assert re.search(
                    rb"\x1b\[(?:\d+;)*38;5;255(?:;\d+)*m", output[frame:]
                ), ("bars are not bright white", bytes(output[frame:]))
                assert not any(chr(ch) in screen for ch in range(0x2800, 0x2900)), (
                    "default bars became braille"
                )
                frames.extend(bars)
                assert_held(source, batch_id, combined)
            assert len(set(frames)) > 1, "active bars did not animate"
            assert_held(source, batch_id, combined)
            wait_for(
                lambda: bool(
                    rows(
                        "SELECT seq FROM events WHERE kind='fixture_tool_finished' AND attempt_id=?",
                        (source,),
                    )
                )
            )
            wait_for(
                lambda: bool(
                    rows(
                        "SELECT text FROM history WHERE kind='assistant' AND attempt_id=? "
                        "AND text LIKE '%Streaming chunk 07.%'",
                        (source,),
                    )
                )
            )
            assert_held(source, batch_id, combined)
            assert all(
                body["mode"] == "after"
                for _, body, _ in receipts("SubmitInputAccumulated")
            )
            assert stop(paused=False) == source
            wait_for(
                lambda: (
                    rows("SELECT state FROM messages WHERE id=?", (batch_id,))
                    == [("processing",)]
                )
            )
            assert rows("SELECT text FROM messages WHERE id=?", (batch_id,)) == [
                (combined,)
            ]
            assert rows(
                "SELECT COUNT(*) FROM attempts WHERE message_id=?", (batch_id,)
            ) == [(1,)]
            boundary = rows(
                "SELECT seq FROM events WHERE kind='attempt_finished' AND attempt_id=? AND payload='cancelled'",
                (source,),
            )[0][0]
            delivery = rows(
                "SELECT seq FROM events WHERE kind='attempt_started' AND payload=?",
                (batch_id,),
            )[0][0]
            assert boundary < delivery, (
                "queued input delivered before forced cancellation committed"
            )
            assert rows(
                "SELECT h.text FROM history h JOIN attempts a ON a.id=h.attempt_id "
                "WHERE h.kind='user' AND a.message_id=?",
                (batch_id,),
            ) == [(combined,)]
            assert_once()

            # Panel/completion Esc only closes UI; only two plain Root Escs Stop.
            active = rows("SELECT id FROM attempts WHERE status='running'")[0][0]
            command("/help")
            keys(b"\x1b", 0.1)
            keys(b"\x1b", 0.1)
            assert rows("SELECT paused FROM session") == [(0,)]
            assert rows("SELECT status FROM attempts WHERE id=?", (active,)) == [
                ("running",)
            ]
            keys(b"/s\t")
            keys(b"\x1b", 0.1)
            keys(b"\x1b", 0.1)
            assert rows("SELECT paused FROM session") == [(0,)]
            assert rows("SELECT status FROM attempts WHERE id=?", (active,)) == [
                ("running",)
            ]
            keys(b"\x15")
            keys(b"\x1b", 0.1)
            assert rows("SELECT paused FROM session") == [(0,)]
            keys(b"x\x7f")  # Ordinary input also disarms a Root Esc.
            keys(b"\x1b", 0.1)
            assert rows("SELECT paused FROM session") == [(0,)]
            assert stop(paused=True, chord=b"\x1b") == active
            assert len(receipts("StopPending")) == 2
            assert not rows(
                "SELECT command FROM command_results WHERE command='\"Stop\"'"
            ), "UI used unchecked Stop"
            pump(3.1)  # Let transient feedback expire before inspecting Root hints.
            stopped_screen = repaint()
            assert not any(
                chr(ch).encode() in plain(stopped_screen)
                for ch in range(0x2581, 0x2589)
            ), "stopped session kept bars animating"
            for obsolete in (
                "/queue",
                "/pause",
                "/resume",
                "/mode",
                "/force",
                "/run",
                "steer /",
            ):
                assert obsolete.encode() not in plain(stopped_screen), (
                    obsolete,
                    plain(stopped_screen),
                )
            keys(b"\r")  # Empty Enter does not resume a stopped session.
            assert rows("SELECT paused FROM session") == [(1,)]
            assert (
                stop(paused=True) is None
            )  # Checked idle Stop acknowledges without cancellation.

            # Root double-Esc with queued input has the same checked cancellation
            # and unpaused automatic FIFO delivery as /stop.
            input_ready()
            submit_plain("escape source")
            escape_source = running("escape source")
            submit_plain("escape first")
            composer("", 0)
            submit_plain("escape second")
            escape_text = "escape first\n\nescape second"
            wait_for(
                lambda: (
                    rows("SELECT text FROM messages WHERE state='queued'")
                    == [(escape_text,)]
                )
            )
            escape_id = rows("SELECT id FROM messages WHERE state='queued'")[0][0]
            assert_held(escape_source, escape_id, escape_text)
            keys(b"\x1b", 0.1)
            assert rows("SELECT status FROM attempts WHERE id=?", (escape_source,)) == [
                ("running",)
            ]
            assert stop(paused=False, chord=b"\x1b") == escape_source
            escape_delivery = running(escape_text)
            assert rows("SELECT text FROM messages ORDER BY seq")[-2:] == [
                ("escape source",),
                (escape_text,),
            ]
            assert rows(
                "SELECT payload FROM events WHERE kind='attempt_started' ORDER BY seq"
            ) == rows(
                "SELECT m.id FROM messages m JOIN attempts a ON a.message_id=m.id ORDER BY m.seq"
            ), "queued delivery violated FIFO"
            assert_once()
            assert (
                stop(paused=True) == escape_delivery
            )  # No queued item: deliberately pause.

            # Ordinary nonempty input resumes on one Enter, with no chooser or /run.
            input_ready()
            submit_plain("edit source")
            editing_source = running("edit source")
            assert rows("SELECT paused FROM session") == [(0,)]
            submitted = receipts("SubmitInputAccumulated")[-1][1]
            assert submitted["resume"] and submitted["expected_paused"], submitted
            submit_plain("pending first")
            composer("", 0)
            submit_plain("pending second")
            authoritative = "pending first\n\npending second"
            wait_for(
                lambda: (
                    rows("SELECT text FROM messages WHERE state='queued'")
                    == [(authoritative,)]
                )
            )
            edit_id = rows("SELECT id FROM messages WHERE state='queued'")[0][0]
            tool_wait(editing_source)
            # Explicit /send consumes its draft exactly once but never steers.
            draft_text = "saved pending draft"
            keys(draft_text.encode(), 0.05)
            command("/draft save")
            wait_for(lambda: rows("SELECT text FROM drafts") == [(draft_text,)])
            composer("", 0)
            input_ready()
            pending_draft_id, draft_revision = rows("SELECT id,revision FROM drafts")[0]
            command(f"/send {pending_draft_id[:8]}")
            authoritative += "\n\n" + draft_text
            wait_for(
                lambda: (
                    rows("SELECT text FROM messages WHERE id=?", (edit_id,))
                    == [(authoritative,)]
                )
            )
            assert not rows("SELECT id FROM drafts")
            draft_receipt = receipts("DispatchDraftAccumulated")[-1]
            assert draft_receipt[1] == {
                "id": pending_draft_id,
                "revision": draft_revision,
                "expected_paused": False,
            }, draft_receipt
            assert_held(editing_source, edit_id, authoritative)
            acquire(edit_id)
            authoritative += "\n\ncommitted edit"
            keys(b"\x15\x1b[200~" + authoritative.encode() + b"\x1b[201~\r", 0.05)
            wait_for(
                lambda: (
                    rows("SELECT text,state FROM messages WHERE id=?", (edit_id,))
                    == [(authoritative, "queued")]
                )
            )
            finish = receipts("FinishPendingEdit")[-1]
            assert finish[1]["id"] == edit_id and finish[1]["text"] == authoritative, (
                finish
            )
            assert not finish[1]["expected_paused"] and finish[2] == "Ack", finish
            wait_for(
                lambda: bool(
                    rows(
                        "SELECT seq FROM events WHERE kind='fixture_tool_finished' AND attempt_id=?",
                        (editing_source,),
                    )
                )
            )
            assert_held(editing_source, edit_id, authoritative)
            shadow = "shadow \u03bb draft"
            keys(shadow.encode() + b"\x01\x1b[C\x1b[C", 0.08)
            composer(shadow, 2)
            acquire(edit_id)
            keys(
                b"\x1b[1;5H", 0.05
            )  # Ctrl+Home reveals the multiline lease's first rows.
            edit_screen = repaint()
            assert_text(edit_screen, "pending first")
            assert_text(edit_screen, "pending second")
            assert "\u250b".encode() not in plain(edit_screen), (
                "leased pending still has dotted preview"
            )
            assert rows("SELECT text FROM messages WHERE id=?", (edit_id,)) == [
                (authoritative,)
            ]
            composer(shadow, 2)  # DB is the shadow, not the leased editor.
            assert rows(
                "SELECT status FROM attempts WHERE id=?", (editing_source,)
            ) == [("running",)]
            assert not rows("SELECT id FROM attempts WHERE status='steered'")
            stop(
                paused=True
            )  # The sole pending item is leased, not queued: pause without delivering it.
            assert not rows("SELECT id FROM attempts WHERE message_id=?", (edit_id,))

            keys(b"\x15lease changes\x1b", 0.12)
            wait_for(
                lambda: (
                    rows("SELECT text,state FROM messages WHERE id=?", (edit_id,))
                    == [(authoritative, "queued")]
                )
            )
            keys(b"X")
            composer("shXadow \u03bb draft", 3)
            keys(b"\x7f")
            composer(shadow, 2)
            acquire(edit_id)
            keys(b"\x15discarded\x1b[B", 0.12)  # Down at the last row cancels.
            wait_for(
                lambda: (
                    rows("SELECT text,state FROM messages WHERE id=?", (edit_id,))
                    == [(authoritative, "queued")]
                )
            )
            keys(b"Y")
            composer("shYadow \u03bb draft", 3)
            keys(b"\x7f")
            composer(shadow, 2)

            # Quit while editing: recovery keeps the shadow/caret, never lease edits.
            acquire(edit_id)
            keys(b"\x15not a recovered composer")
            composer(shadow, 2)
            before_reopen = rows("SELECT id FROM attempts ORDER BY id")
            command("/quit")
            exited()
            start(slow_config)
            composer(shadow, 2)
            assert rows("SELECT paused FROM session") == [(1,)]
            assert rows("SELECT text,state FROM messages WHERE id=?", (edit_id,)) == [
                (authoritative, "queued")
            ]
            assert rows("SELECT id FROM attempts ORDER BY id") == before_reopen
            keys(b"Z")
            composer("shZadow \u03bb draft", 3)
            keys(b"\x7f")
            composer(shadow, 2)
            acquire(edit_id)
            keys(b"\x15\r")  # Empty edit Enter deletes instead of dispatching.
            wait_for(
                lambda: (
                    rows("SELECT state FROM messages WHERE id=?", (edit_id,))
                    == [("cancelled",)]
                )
            )
            composer(shadow, 2)
            assert rows("SELECT paused FROM session") == [(1,)]
            assert not rows("SELECT id FROM attempts WHERE message_id=?", (edit_id,))

            # Nonempty edit Enter after Stop atomically replaces, resumes and sends.
            input_ready()
            keys(b"\x15", 0.05)
            submit_plain("resume source")
            resume_source = running("resume source")
            tool_wait(resume_source)
            submit_plain("replace pending")
            composer("", 0)
            replacement_id = rows("SELECT id FROM messages WHERE state='queued'")[0][0]
            keys(shadow.encode() + b"\x01\x1b[C\x1b[C", 0.08)
            acquire(replacement_id)
            stop(paused=True)  # Again, only an editing lease remains, so Stop pauses.
            replacement = "edited \u03bb payload"
            keys(b"\x15" + replacement.encode() + b"\r", 0.05)
            wait_for(
                lambda: (
                    rows(
                        "SELECT m.text,m.state,s.paused FROM messages m CROSS JOIN session s "
                        "WHERE m.id=?",
                        (replacement_id,),
                    )
                    == [(replacement, "processing", 0)]
                )
            )
            composer(shadow, 2)
            finish = receipts("FinishPendingEdit")[-1]
            assert (
                finish[1]["id"] == replacement_id and finish[1]["text"] == replacement
            )
            assert finish[1]["expected_paused"] and finish[2] == "Ack", finish
            assert rows(
                "SELECT COUNT(*) FROM attempts WHERE message_id=?", (replacement_id,)
            ) == [(1,)]
            assert_once()

            # SIGTERM during active work restores modes and durably cancels it.
            active = rows("SELECT id FROM attempts WHERE status='running'")[0][0]
            command("/settings status braille --default")
            wait_notice("status set to braille (default)")
            braille = len(output)
            wait_for(
                lambda: any(
                    chr(ch).encode() in plain(braille) for ch in range(0x2800, 0x2900)
                ),
                full_repaint=True,
            )
            assert not any(
                chr(ch).encode() in plain(braille) for ch in range(0x2581, 0x2589)
            ), "explicit braille preference rendered bars"
            child.terminate()
            exited()
            wait_notice("status set to braille (default)")
            assert rows("SELECT status FROM attempts WHERE id=?", (active,)) == [
                ("cancelled",)
            ]
            composer(shadow, 2)

            # Searchable quit picker accepts only; the second Enter really exits.
            start(slow_config)
            keys(b"\x15/qui\x13\x1b[B\r")
            assert child.poll() is None, "completion executed /quit"
            composer("/quit ", 6)
            keys(b"\r", 0)
            exited()
            start(slow_config)
            keys(b"\x15/quit\r", 0)  # Fully typed quit needs only one Enter.
            exited()
            start(slow_config)
            keys(b"confirmed exit source\r", 0.05)
            active = running("confirmed exit source")
            keys(b"\x03")
            assert child.poll() is None
            keys(b"\x03", 0)
            exited()
            assert rows("SELECT status FROM attempts WHERE id=?", (active,)) == [
                ("cancelled",)
            ]
            assert not rows("SELECT id FROM attempts WHERE status='steered'"), (
                "user cancellation was replaced by steering"
            )
            assert_once()
            assert b"am nyam" in output
            assert output.count(b"\x1b[?1049l") >= output.count(b"\x1b[?1049h"), (
                "reader not restored"
            )
            # Restore may emit one redundant EndSynchronizedUpdate before leaving.
            synchronized = False
            for match in re.finditer(rb"\x1b\[\?2026([hl])", bytes(output)):
                if match[1] == b"h":
                    assert not synchronized, "nested synchronized update"
                    synchronized = True
                else:
                    assert synchronized or bytes(output[match.end() :]).startswith(
                        b"\x1b[?1049l"
                    ), "unmatched synchronized end"
                    synchronized = False
            assert not synchronized, "synchronized update left open"
            print(
                "PTY smoke passed: stop-only catalog, modified arrows/UTF-8 composer, help/settings/defaults, "
                "completion/pinned fields/drafts, bindings/macros, history/recall, Stop/auto-resume/root double-Esc, "
                "default white animated bars/saved braille, full-turn accumulation/no steering/no duplicates, "
                "checked Stop receipts/queued FIFO delivery, pending acquisition/preview hiding/cancel/delete/atomic resume, "
                "shadow recovery, quit/Ctrl+C/SIGTERM/terminal restoration. Offline fixture; not an emulator."
            )
        finally:
            if child is not None and child.poll() is None:
                child.kill()
                child.wait()
            os.close(master)
            os.close(slave)


if __name__ == "__main__":
    main()
