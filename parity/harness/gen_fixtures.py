#!/usr/bin/env python3
"""Deterministic synthetic fixture generator for the less/rust-less parity harness.

No timestamps, no randomness, no PII. Every fixture is byte-for-byte
reproducible across runs (re-running this script regenerates identical
files). Invoked by gen-fixtures.sh; see parity/harness/CONTRACT.md for the
fixture directory contract (parity/fixtures/, synthetic-only).
"""
import os

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES_DIR = os.path.normpath(os.path.join(HERE, "..", "fixtures"))


def write_text(name: str, content: str) -> None:
    path = os.path.join(FIXTURES_DIR, name)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(content)
    print(f"wrote {path} ({len(content.encode('utf-8'))} bytes)")


def write_bytes(name: str, content: bytes) -> None:
    path = os.path.join(FIXTURES_DIR, name)
    with open(path, "wb") as f:
        f.write(content)
    print(f"wrote {path} ({len(content)} bytes)")


def gen_lines200() -> None:
    lines = []
    for i in range(1, 201):
        if i == 20:
            lines.append(f"{i:03d} line {i} of 200 :: ALPHA_TOKEN appears here")
        elif i == 80:
            lines.append(f"{i:03d} line {i} of 200 :: ALPHA_TOKEN appears here too")
        elif i == 140:
            lines.append(f"{i:03d} line {i} of 200 :: ALPHA_TOKEN appears a third time")
        elif i == 111:
            lines.append(f"{i:03d} line {i} of 200 :: SOLO_NEEDLE unique occurrence")
        elif i == 60:
            lines.append(f"{i:03d} line {i} of 200 :: regex sample cat bat hat sat mat")
        elif i == 61:
            lines.append(f"{i:03d} line {i} of 200 :: regex sample foobar123 foobaz456")
        else:
            lines.append(f"{i:03d} line {i} of 200 :: plain filler content")
    lines.append("")  # trailing newline
    write_text("lines200.txt", "\n".join(lines))


def gen_longlines() -> None:
    def digits(n: int) -> str:
        s = "0123456789"
        return (s * (n // 10 + 1))[:n]

    line_80 = "L80|" + digits(76)  # 4 + 76 = 80 chars exactly
    assert len(line_80) == 80, len(line_80)
    line_81 = "L81|" + digits(77)  # 81 chars: one past the boundary
    assert len(line_81) == 81, len(line_81)
    line_long = "LONG|" + digits(290) + "|RIGHTMOST-MARKER-END"
    rows: list[str] = [
        "short line before the long ones",
        line_80,
        line_81,
        line_long,
        "short line after the long ones",
    ]
    rows.append("")
    write_text("longlines.txt", "\n".join(rows))


def gen_tabs_ctrl() -> None:
    parts = []
    parts.append("\tindented with one leading tab")
    parts.append("before\tafter\ttab-separated fields")
    parts.append("a\tb\tc\td\tfour tabs")
    parts.append(
        "ctrl:[" + "\x01" + "][" + "\x02" + "][" + "\x07" + "][" + "\x1f" + "]end"
    )
    parts.append("plain line with no special characters")
    content = "\n".join(parts) + "\n"
    write_bytes("tabs-ctrl.txt", content.encode("utf-8"))


def gen_ansi() -> None:
    ESC = "\x1b"
    rows = []
    rows.append(f"{ESC}[31mRed text{ESC}[0m normal")
    rows.append(f"{ESC}[1;32mBold Green{ESC}[0m and {ESC}[34mBlue{ESC}[0m")
    rows.append(f"{ESC}[35mMagenta start (unclosed)")
    rows.append("next line after unclosed color (should reset per-line under -R)")
    rows.append(f"non-color esc sequence: {ESC}[2;5Hnon-color-esc-seq-here end")
    rows.append(
        f"osc8 esc-terminated: {ESC}]8;;https://example.invalid/{ESC}\\link text{ESC}]8;;{ESC}\\ trailing"
    )
    rows.append(
        f"osc8 bel-terminated: {ESC}]8;;https://example.invalid/\x07link2{ESC}]8;;\x07 trailing2"
    )
    content = "\n".join(rows) + "\n"
    write_bytes("ansi.txt", content.encode("utf-8"))


def gen_utf8_mixed() -> None:
    rows = []
    rows.append("こんにちは世界 (Japanese greeting)")
    rows.append("emoji line: 🎉🚀✅ end")
    rows.append("hello 世界 world (mixed ascii + CJK)")
    rows.append("café résumé naïve (accented latin, narrow width)")
    rows.append("漢" * 45)  # wide CJK repeated: 45 * 2 cols = 90 > 80-col terminal
    rows.append("plain ascii tail line")
    content = "\n".join(rows) + "\n"
    write_bytes("utf8-mixed.txt", content.encode("utf-8"))


def gen_utf8_malformed() -> None:
    chunks = []
    chunks.append(b"line before\n")
    chunks.append(b"\xe4\xb8")  # truncated 3-byte UTF-8 sequence (missing final byte)
    chunks.append(b"\n")
    chunks.append(b"line after\n")
    write_bytes("utf8-malformed.txt", b"".join(chunks))


def gen_multi_files() -> None:
    for idx in (1, 2, 3):
        lines = [f"multi file {idx} - line {n}" for n in range(1, 6)]
        lines.append("")
        write_text(f"multi-{idx}.txt", "\n".join(lines))


def gen_short() -> None:
    lines = [f"short file - line {n} of 5" for n in range(1, 6)]
    lines.append("")
    write_text("short.txt", "\n".join(lines))


def gen_large() -> None:
    lines = [f"{i:05d} large file line {i} of 5000 :: filler content" for i in range(1, 5001)]
    lines.append("")
    write_text("large.txt", "\n".join(lines))


def main() -> None:
    os.makedirs(FIXTURES_DIR, exist_ok=True)
    gen_lines200()
    gen_longlines()
    gen_tabs_ctrl()
    gen_ansi()
    gen_utf8_mixed()
    gen_utf8_malformed()
    gen_multi_files()
    gen_short()
    gen_large()


if __name__ == "__main__":
    main()
