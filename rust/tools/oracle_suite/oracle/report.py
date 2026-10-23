"""Markdown report and side-by-side contact sheets."""

from __future__ import annotations

from pathlib import Path

from .checks import CheckResult

try:
    from PIL import Image, ImageDraw
except ImportError:  # pragma: no cover - Pillow is optional for text-only reports
    Image = ImageDraw = None

THUMB = (640, 360)


def _thumb(path: Path) -> "Image.Image | None":
    if Image is None or not path.is_file():
        return None
    image = Image.open(path).convert("RGB")
    image.thumbnail(THUMB, Image.LANCZOS)
    return image


def make_sheet(original: Path, bevy: Path, output: Path, label: str) -> bool:
    if Image is None:
        return False
    left = _thumb(original)
    right = _thumb(bevy)
    if left is None and right is None:
        return False
    width = THUMB[0] * 2
    height = THUMB[1] + 28
    sheet = Image.new("RGB", (width, height), (24, 24, 24))
    draw = ImageDraw.Draw(sheet)
    draw.text((8, 7), f"{label}    original 1.2 (left)  |  Bevy aa-client (right)",
              fill=(240, 240, 240))
    if left is not None:
        sheet.paste(left, (THUMB[0] - left.width, 28))
    if right is not None:
        sheet.paste(right, (THUMB[0] + (THUMB[0] - right.width), 28))
    output.parent.mkdir(parents=True, exist_ok=True)
    sheet.save(output)
    return True


def scenario_report(path: Path, scenario: dict, original, bevy, results: list[CheckResult],
                    sheets: list[Path], notes: list[str]) -> None:
    lines = [f"# {scenario['name']}", "", scenario.get("description", ""), "",
             f"- original: `{original}`", f"- Bevy: `{bevy}`", "",
             "| check | original | Bevy | result |", "|---|---|---|---|"]
    for result in results:
        lines.append(f"| {result.name} | {_cell(result.original)} | {_cell(result.bevy)} | "
                     f"**{result.status}** {result.detail} |")
    lines.append("")
    if sheets:
        lines.append("## Screenshots (original left, Bevy right)")
        lines.append("")
        for sheet in sheets:
            lines.append(f"### {sheet.stem}")
            lines.append("")
            lines.append(f"![{sheet.stem}]({sheet.relative_to(path.parent).as_posix()})")
            lines.append("")
    if notes:
        lines.append("## Notes")
        lines.append("")
        lines.extend(f"- {note}" for note in notes)
        lines.append("")
    path.write_text("\n".join(lines), encoding="utf-8")


def index_report(path: Path, scenarios: list[tuple[str, Path, list[CheckResult]]], notes: list[str]) -> None:
    lines = ["# Oracle suite report", "", "Original ArcheAge 1.2 (hidden, aaemu-clientdriver) vs Bevy "
             "`aa-client` on one lane server.", ""]
    lines.append("| scenario | SAME | DIFFERENT | NOT RUN | report |")
    lines.append("|---|---|---|---|---|")
    for name, report, results in scenarios:
        counts = {status: sum(1 for result in results if result.status == status)
                  for status in ("SAME", "DIFFERENT", "NOT RUN")}
        lines.append(f"| {name} | {counts['SAME']} | {counts['DIFFERENT']} | "
                     f"{counts['NOT RUN']} | [{report.name}]({report.relative_to(path.parent).as_posix()}) |")
    lines.append("")
    if notes:
        lines.append("## Notes")
        lines.append("")
        lines.extend(f"- {note}" for note in notes)
        lines.append("")
    path.write_text("\n".join(lines), encoding="utf-8")


def _cell(text: str) -> str:
    text = (text or "-").replace("|", "\\|").replace("\n", " ")
    return text if len(text) <= 160 else text[:157] + "..."
